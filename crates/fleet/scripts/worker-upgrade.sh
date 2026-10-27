#!/usr/bin/env bash
# Upgrades one stopped runner in place on a macOS or Linux worker, using the
# runner's own self-update layout: the release goes to bin.VERSION and
# externals.VERSION inside the exact runner directory, and bin and externals
# become symlinks to them. A runner still on plain bin/ and externals/ folders
# is converted the way its self-update does (bin -> bin.OLDVERSION). The old
# versioned folders stay as the rollback copy until the runner's own next
# self-update prunes them. The directory is never moved, and .runner,
# .credentials* and the configuration are never touched. A swap marker is
# written before the first change and removed after the last, so an
# interrupted swap is rolled back on the next run. The controller supplies
# RF_* values on SSH stdin and starts the service after.
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
version_ok() { case "$1" in ''|*[!0-9.]*|.*|*.|*..*) return 1 ;; *.*.*) return 0 ;; *) return 1 ;; esac; }
version_ok "$RF_VERSION" || { echo "refusing unexpected runner version: $RF_VERSION" >&2; exit 2; }
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

# Anything started from the runner directory (its listener, a job, or its
# self-update script) means it isn't stopped. The list is read in full first,
# so the search (whose arguments contain the pattern) isn't in it. Checked
# again right before the swap.
check_stopped() {
  local processes
  processes="$(ps -A -ww -o command= 2>/dev/null || true)"
  if printf '%s\n' "$processes" | grep -F -- "$root/bin" | grep -F -- "Runner.Worker" >/dev/null; then
    echo "a job is running in $root; refusing to upgrade $RF_NAME" >&2
    exit 5
  fi
  if printf '%s\n' "$processes" | grep -F -- "$root/" >/dev/null; then
    echo "$RF_NAME is still running (a process from $root, such as its service or a self-update); it must be stopped before upgrading" >&2
    exit 5
  fi
}
check_stopped
# A self-update writes _work/_update.sh, then update.finished when it's done.
# A recent script without a newer update.finished may still be swapping.
update_script="$root/_work/_update.sh"
if [ -f "$update_script" ] && [ -n "$(find "$update_script" -mmin -30 -print 2>/dev/null)" ] \
  && ! { [ -f "$root/update.finished" ] && [ "$root/update.finished" -nt "$update_script" ]; }; then
  echo "a runner self-update of $RF_NAME started in the last 30 minutes and hasn't finished; try again later" >&2
  exit 5
fi
# A cleanup holds the maintenance lock while it works; the upgrade takes it
# too, until it ends. A lock is live while any recorded process or process
# group still exists (a recycled PID keeps it live: that fails closed).
lock="$root/.fleet-maintenance"
lock_live() {
  local pid
  for pid in $(cat "$lock" 2>/dev/null); do
    case "$pid" in ''|*[!0-9]*) continue ;; esac
    if kill -0 "$pid" 2>/dev/null || kill -0 -- "-$pid" 2>/dev/null; then return 0; fi
  done
  return 1
}
if [ -f "$lock" ]; then
  if lock_live; then
    echo "a cleanup or another upgrade is still working in $root (its processes are recorded in $lock); try again when it has finished. If those processes are unrelated, remove that file by hand" >&2
    exit 5
  fi
  rm -f -- "$lock"
fi
printf '%s' "$$" > "$lock"
release_lock() { rm -f -- "$lock"; }
trap release_lock EXIT

marker="$root/.fleet-upgrade-swap"
# Where bin or externals points, as a folder name in the root: `bin.2.329.0`,
# or `bin` itself for a plain folder. Fails for a link that leaves the root.
target_of() {
  local d="$1" link real
  if [ -L "$root/$d" ]; then
    link="$(readlink "$root/$d")"
    case "$link" in /*) ;; *) link="$root/$link" ;; esac
    real="$(cd "$link" 2>/dev/null && pwd -P)" || return 1
    [ "$(dirname "$real")" = "$root" ] || return 1
    case "$(basename "$real")" in "$d".*) ;; *) return 1 ;; esac
    basename "$real"
  elif [ -d "$root/$d" ]; then
    echo "$d"
  else
    return 1
  fi
}
point() { ln -sfn "$root/$2" "$root/$1"; }

# The marker names, for bin and externals, the folder that held the old
# version; rolling back points each link back at it. That covers both a link
# that was already moved and a plain folder that was renamed but not yet
# linked.
restore_marker() {
  local d previous failed=0
  while IFS='=' read -r d previous; do
    case "$d" in bin|externals) ;; *) continue ;; esac
    case "$previous" in "$d".*) ;; *) continue ;; esac
    # Still the plain folder it was: this one was never moved.
    if [ -d "$root/$d" ] && [ ! -L "$root/$d" ]; then continue; fi
    if [ ! -d "$root/$previous" ] || [ -L "$root/$previous" ]; then
      failed=1
      continue
    fi
    if [ -L "$root/$d" ] || [ ! -e "$root/$d" ]; then
      point "$d" "$previous" || failed=1
    fi
    [ "$(target_of "$d" 2>/dev/null || true)" = "$previous" ] || failed=1
  done < "$marker"
  # The marker stays until every link is back, so the next run tries again.
  [ "$failed" = 0 ] || return 1
  rm -f -- "$marker"
}

# Roll back a swap an earlier run didn't finish (the connection dropped).
if [ -f "$marker" ]; then
  if restore_marker; then
    echo "rolled back an interrupted upgrade of $RF_NAME"
  else
    echo "an earlier upgrade of $RF_NAME was interrupted and couldn't be rolled back: with its service stopped, point bin and externals at the folders named in $marker (ln -sfn), remove the marker, then upgrade again" >&2
    exit 3
  fi
fi

old_bin="$(target_of bin)" || { echo "$root/bin is missing or points outside the runner directory; repair it by hand" >&2; exit 3; }
old_externals="$(target_of externals)" || { echo "$root/externals is missing or points outside the runner directory; repair it by hand" >&2; exit 3; }

current="$("$root/bin/Runner.Listener" --version 2>/dev/null | tail -n 1 || true)"
if [ "$current" = "$RF_VERSION" ]; then
  echo "$RF_NAME already runs $RF_VERSION"
  exit 0
fi
if { [ "$old_bin" = bin ] || [ "$old_externals" = externals ]; } && ! version_ok "$current"; then
  echo "can't tell which version $RF_NAME runs ('$current'), so its folders can't be kept as a rollback copy; refusing" >&2
  exit 3
fi
for name in "bin.$RF_VERSION" "externals.$RF_VERSION"; do
  if [ -L "$root/$name" ]; then
    echo "$root/$name is a symlink; refusing" >&2
    exit 3
  fi
done
# The folder that will hold the current version once the links move.
held_name() {
  if [ "$2" = "$1" ]; then echo "$1.$current"; else echo "$2"; fi
}
keep_bin="$(held_name bin "$old_bin")"
keep_externals="$(held_name externals "$old_externals")"
for pair in "bin:$old_bin:$keep_bin" "externals:$old_externals:$keep_externals"; do
  IFS=: read -r d old keep <<< "$pair"
  if [ "$old" = "$d" ] && [ -e "$root/$keep" ]; then
    echo "$root/$d is a plain folder but $root/$keep already exists; inspect both before upgrading" >&2
    exit 3
  fi
  if [ "$keep" = "$d.$RF_VERSION" ]; then
    echo "$RF_NAME's $d already points at $d.$RF_VERSION; nothing to swap" >&2
    exit 3
  fi
done

remove_dir() {
  find "$1" -xdev -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null || true
  find "$1" -xdev -depth -delete
}

stage="$(mktemp -d "$root/.fleet-upgrade.XXXXXX")"
swapping=0
finished=0
finish() {
  local status=$?
  # The marker's removal is the moment the upgrade completes; while it's
  # there, the swap isn't done and is rolled back.
  if [ "$finished" != 1 ] && [ "$swapping" = 1 ] && [ -f "$marker" ]; then
    if restore_marker; then
      echo "restored the previous bin and externals of $RF_NAME" >&2
    else
      echo "couldn't restore the previous bin and externals of $RF_NAME; they're named in $marker" >&2
      [ "$status" != 0 ] || status=3
    fi
  fi
  remove_dir "$stage" 2>/dev/null || true
  release_lock
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

# Put the new folders in place beside the current ones; nothing points at them yet.
for d in bin externals; do
  if [ -e "$root/$d.$RF_VERSION" ]; then remove_dir "$root/$d.$RF_VERSION"; fi
  mv "$stage/new/$d" "$root/$d.$RF_VERSION"
done

# The swap: nothing may have started from the directory meanwhile; record
# where each link pointed, then move both.
check_stopped
printf 'bin=%s\nexternals=%s\n' "$keep_bin" "$keep_externals" > "$marker"
swapping=1
for d in bin externals; do
  previous="$(target_of "$d")"
  if [ "$previous" = "$d" ]; then
    mv "$root/$d" "$root/$d.$current"
  fi
  point "$d" "$d.$RF_VERSION"
done
# Complete: removing the marker is the commit point.
rm -f -- "$marker"
finished=1

# Refresh the root files a self-update copies (the package's top-level scripts
# and templates, and runsvc.sh from bin), each through a temporary file and a
# rename so a file is never left half-written.
replace_file() {
  local from="$1" name="$2"
  [ -d "$root/$name" ] && return 1
  cp -p "$from" "$root/.$name.fleet-new" && mv -f "$root/.$name.fleet-new" "$root/$name"
}
stale=""
while IFS= read -r file; do
  name="${file##*/}"
  case "$name" in .*|*/*) continue ;; *.sh|*.cmd|*.template) ;; *) continue ;; esac
  replace_file "$file" "$name" || stale="$stale $name"
done <<< "$(find "$stage/new" -mindepth 1 -maxdepth 1 -type f -print 2>/dev/null)"
if [ -f "$root/runsvc.sh" ] && [ -f "$root/bin/runsvc.sh" ]; then
  replace_file "$root/bin/runsvc.sh" runsvc.sh || stale="$stale runsvc.sh"
fi
if [ -n "$stale" ]; then
  echo "switched $RF_NAME to $RF_VERSION, but couldn't refresh:$stale" >&2
fi
echo "switched $RF_NAME to $RF_VERSION (bin -> bin.$RF_VERSION); the previous version stays beside it as the rollback copy"
