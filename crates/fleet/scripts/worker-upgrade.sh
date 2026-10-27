#!/usr/bin/env bash
# Upgrades one stopped runner in place on a macOS or Linux worker: downloads
# the release, verifies the SHA-256 digest GitHub publishes, and replaces bin/
# and externals/ inside the exact runner directory. The directory is never
# moved, and .runner, .credentials* and the configuration are never touched.
# The replaced bin/ and externals/ are kept as bin.fleet-previous and
# externals.fleet-previous; a failure while swapping puts them back. The
# controller supplies RF_* values on SSH stdin and starts the service after.
set -euo pipefail
set -f

: "${RF_ROOT:?}"
: "${RF_NAME:?}"
: "${RF_ASSET_URL:?}"
: "${RF_ASSET_NAME:?}"
: "${RF_CHECKSUM:?}"
: "${RF_VERSION:?}"

case "$RF_ASSET_URL" in
  https://github.com/actions/runner/releases/download/*) ;;
  *) echo "refusing unexpected runner asset URL: $RF_ASSET_URL" >&2; exit 2 ;;
esac
case "$RF_ROOT" in
  "$HOME/gh-runners"/*) ;;
  *) echo "refusing root outside $HOME/gh-runners: $RF_ROOT" >&2; exit 2 ;;
esac
if [ -L "$RF_ROOT" ] || [ ! -f "$RF_ROOT/.runner" ]; then
  echo "no configured runner at exact path $RF_ROOT" >&2
  exit 3
fi
if ! tr -d '[:space:]' < "$RF_ROOT/.runner" | grep -F "\"agentName\":\"$RF_NAME\"" >/dev/null; then
  echo "runner name at exact path does not match $RF_NAME" >&2
  exit 3
fi
root="$(cd "$RF_ROOT" && pwd -P)"
if [ -f /etc/NIXOS ]; then
  export LD_LIBRARY_PATH="/run/current-system/sw/share/nix-ld/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi

processes() { ps -A -o command= 2>/dev/null; }
if processes | grep -F -- "$root/bin/Runner.Worker" >/dev/null; then
  echo "a job is running in $root; refusing to upgrade $RF_NAME" >&2
  exit 5
fi
if processes | grep -F -- "$root/bin/Runner.Listener" >/dev/null; then
  echo "$RF_NAME is still running; its service must be stopped before upgrading" >&2
  exit 5
fi
for d in bin externals; do
  if [ -L "$root/$d" ] && [ ! -d "$root/$d" ]; then
    echo "$root/$d is a dangling symlink; refusing" >&2
    exit 3
  fi
done

current="$("$root/bin/Runner.Listener" --version 2>/dev/null | tail -n 1 || true)"
if [ "$current" = "$RF_VERSION" ]; then
  echo "$RF_NAME already runs $RF_VERSION"
  exit 0
fi

remove_dir() {
  find "$1" -xdev -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null || true
  find "$1" -xdev -depth -delete
}

stage="$(mktemp -d "$root/.fleet-upgrade.XXXXXX")"
swapping=0
finished=0
restore() {
  local d
  for d in bin externals; do
    if [ -d "$root/$d.fleet-previous" ] && [ ! -L "$root/$d.fleet-previous" ]; then
      if [ -e "$root/$d" ] || [ -L "$root/$d" ]; then mv "$root/$d" "$stage/failed-$d"; fi
      mv "$root/$d.fleet-previous" "$root/$d"
    fi
  done
}
finish() {
  local status=$?
  if [ "$finished" != 1 ] && [ "$swapping" = 1 ]; then
    if restore; then
      echo "restored the previous bin/ and externals/ of $RF_NAME" >&2
    else
      echo "couldn't restore the previous files; they're in $root/bin.fleet-previous and $root/externals.fleet-previous" >&2
    fi
  fi
  remove_dir "$stage" 2>/dev/null || true
  exit "$status"
}
trap finish EXIT
trap 'exit 130' HUP INT TERM

archive="$stage/runner.tar.gz"
echo "downloading $RF_ASSET_NAME"
curl --fail --location --silent --show-error "$RF_ASSET_URL" --output "$archive"
if command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$archive" | awk '{print $1}')"
else
  echo "cannot verify GitHub SHA-256 digest: shasum/sha256sum not found" >&2
  exit 4
fi
if [ "$actual" != "$RF_CHECKSUM" ]; then
  echo "SHA-256 mismatch for $RF_ASSET_NAME" >&2
  exit 4
fi
echo "verified GitHub SHA-256 digest"

mkdir "$stage/new"
tar -xzf "$archive" -C "$stage/new"
if [ ! -x "$stage/new/bin/Runner.Listener" ] || [ ! -d "$stage/new/externals" ]; then
  echo "$RF_ASSET_NAME does not contain bin/Runner.Listener and externals/" >&2
  exit 4
fi
new="$("$stage/new/bin/Runner.Listener" --version 2>/dev/null | tail -n 1 || true)"
if [ "$new" != "$RF_VERSION" ]; then
  echo "the downloaded runner reports version '$new', expected $RF_VERSION" >&2
  exit 4
fi

for d in bin externals; do
  previous="$root/$d.fleet-previous"
  if [ -L "$previous" ]; then
    echo "refusing to remove $previous: it is a symlink" >&2
    exit 3
  fi
  if [ -e "$previous" ]; then remove_dir "$previous"; fi
done

swapping=1
mv "$root/bin" "$root/bin.fleet-previous"
mv "$stage/new/bin" "$root/bin"
mv "$root/externals" "$root/externals.fleet-previous"
mv "$stage/new/externals" "$root/externals"
finished=1
echo "replaced bin/ and externals/ of $RF_NAME with $RF_VERSION; the previous files are in bin.fleet-previous and externals.fleet-previous"
