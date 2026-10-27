#!/usr/bin/env bash
# Disk cleanup on a macOS or Linux worker. The controller sets RF_MODE
# (survey: read-only, or clean) and RF_DIAG_DAYS, then calls
#   runner_target KIND RUNNER ROOT EXPECTED [FILE...]
#   machine_target KIND EXPECTED
# once per target. Every path is derived here from a fixed suffix of the exact
# runner directory (or the home directory) and must resolve to exactly that
# place: no symlink below it, nothing outside it, no globs. Directory contents
# are removed with find -delete, which never follows links or crosses
# filesystems. .runner, .credentials*, config files, bin and externals are
# never targets. Rows: RFC (survey), RFF (survey: a log file), RFX (removed),
# RFE (refused or failed).
set -uo pipefail
set -f

: "${RF_MODE:?}"
: "${RF_DIAG_DAYS:?}"
case "$RF_MODE" in survey|clean) ;; *) echo "invalid RF_MODE" >&2; exit 2 ;; esac
case "$RF_DIAG_DAYS" in ''|*[!0-9]*) echo "invalid RF_DIAG_DAYS" >&2; exit 2 ;; esac

tab="$(printf '\t')"
row() { local IFS="$tab"; printf '%s\n' "$*"; }

processes() { ps -A -o command= 2>/dev/null; }
# A job runs while a Runner.Worker process from the runner's bin/ is alive.
job_in() { processes | grep -F -- "$1/bin/Runner.Worker" >/dev/null; }
any_job() { processes | grep -F -- "/bin/Runner.Worker" >/dev/null; }

kib() { du -skx "$1" 2>/dev/null | awk 'END { print $1 + 0 }'; }

# Prints BASE/SUFFIX resolved, when it is a directory, no component below BASE
# is a symlink, and it resolves to exactly BASE/SUFFIX. Returns 1 when it
# doesn't exist and 2 when it escapes.
resolve() {
  local base="$1" suffix="$2" path part real
  path="$base"
  local IFS=/
  for part in $suffix; do
    path="$path/$part"
    if [ -L "$path" ]; then return 2; fi
  done
  [ -d "$path" ] || return 1
  real="$(cd "$path" && pwd -P)" || return 1
  [ "$real" = "$base/$suffix" ] || return 2
  printf '%s\n' "$real"
}

# Removes everything inside DIR, never DIR itself.
empty_dir() {
  find "$1" -xdev -mindepth 1 -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null
  find "$1" -xdev -mindepth 1 -delete
}

# Removes DIR and everything in it. Only used for the upgrade's own backups.
remove_dir() {
  find "$1" -xdev -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null
  find "$1" -xdev -depth -delete
}

suffix_for() {
  case "$1" in
    work-temp) echo "_work/_temp" ;;
    work-actions) echo "_work/_actions" ;;
    work) echo "_work" ;;
    diag) echo "_diag" ;;
    previous-bin) echo "bin.fleet-previous" ;;
    previous-externals) echo "externals.fleet-previous" ;;
    *) return 1 ;;
  esac
}

old_logs() {
  find "$1" -mindepth 1 -maxdepth 1 -type f \( -name 'Runner_*.log' -o -name 'Worker_*.log' \) \
    -mtime "+$RF_DIAG_DAYS" -print | sed 's|.*/||' | sort
}

runner_target() {
  local kind="$1" runner="$2" root="$3" expected="$4"
  shift 4
  local suffix real_root real status size
  refuse() { row RFE "$kind" "$runner" "$1"; }
  suffix="$(suffix_for "$kind")" || { refuse "unknown cleanup target $kind"; return; }
  if [ -L "$root" ] || [ ! -f "$root/.runner" ]; then
    [ "$RF_MODE" = survey ] && return
    refuse "no configured runner at $root"; return
  fi
  if ! tr -d '[:space:]' < "$root/.runner" | grep -F "\"agentName\":\"$runner\"" >/dev/null; then
    refuse "the runner at $root is not $runner"; return
  fi
  real_root="$(cd "$root" && pwd -P)" || { refuse "can't resolve $root"; return; }
  real="$(resolve "$real_root" "$suffix")"
  status=$?
  if [ "$status" = 1 ]; then
    [ "$RF_MODE" = survey ] && return
    refuse "$real_root/$suffix no longer exists"; return
  elif [ "$status" != 0 ]; then
    refuse "$real_root/$suffix is a symlink or resolves outside $real_root; left it alone"; return
  fi

  if [ "$RF_MODE" = survey ]; then
    if [ "$kind" = diag ]; then
      local logs f count=0 total=0
      logs="$(old_logs "$real")"
      [ -n "$logs" ] || return
      while IFS= read -r f; do
        count=$((count + 1))
        total=$((total + $(kib "$real/$f")))
        row RFF "$runner" "$f"
      done <<< "$logs"
      row RFC "$kind" "$runner" "$real" "$total" "$count" ""
    else
      case "$kind" in
        previous-*) ;;
        *) [ -n "$(find "$real" -mindepth 1 -maxdepth 1 -print 2>/dev/null | head -n 1)" ] || return ;;
      esac
      row RFC "$kind" "$runner" "$real" "$(kib "$real")" "" ""
    fi
    return
  fi

  if [ "$real" != "$expected" ]; then
    refuse "$real_root/$suffix resolves to $real, not the reviewed $expected; left it alone"; return
  fi
  if job_in "$real_root"; then
    refuse "a job is running in $real_root; left it alone"; return
  fi
  case "$kind" in
    diag)
      local f removed=0 freed=0
      for f in "$@"; do
        case "$f" in
          */*|.*) refuse "unexpected log name $f"; continue ;;
          Runner_*.log|Worker_*.log) ;;
          *) refuse "unexpected log name $f"; continue ;;
        esac
        if [ -f "$real/$f" ] && [ ! -L "$real/$f" ] \
          && [ -n "$(find "$real/$f" -maxdepth 0 -mtime "+$RF_DIAG_DAYS" -print)" ]; then
          size="$(kib "$real/$f")"
          if rm -f -- "$real/$f"; then
            removed=$((removed + 1))
            freed=$((freed + size))
          fi
        fi
      done
      row RFX "$kind" "$runner" "$real" "$freed" "$removed" ""
      ;;
    previous-*)
      size="$(kib "$real")"
      if remove_dir "$real"; then row RFX "$kind" "$runner" "$real" "$size" "" ""; else refuse "couldn't remove all of $real"; fi
      ;;
    *)
      size="$(kib "$real")"
      if empty_dir "$real"; then row RFX "$kind" "$runner" "$real" "$size" "" ""; else refuse "couldn't remove everything inside $real"; fi
      ;;
  esac
}

machine_target() {
  local kind="$1" expected="$2" runner=""
  refuse() { row RFE "$kind" "$runner" "$1"; }
  if [ "$RF_MODE" = clean ] && any_job; then
    refuse "a job is running on this machine; left machine-wide caches alone"; return
  fi
  case "$kind" in
    xcode-derived-data)
      [ "$(uname -s)" = Darwin ] || return
      local home real status size
      home="$(cd "$HOME" && pwd -P)" || { refuse "can't resolve the home directory"; return; }
      real="$(resolve "$home" "Library/Developer/Xcode/DerivedData")"
      status=$?
      if [ "$status" = 1 ]; then
        [ "$RF_MODE" = survey ] && return
        refuse "$home/Library/Developer/Xcode/DerivedData no longer exists"; return
      elif [ "$status" != 0 ]; then
        refuse "DerivedData is a symlink or resolves outside $home; left it alone"; return
      fi
      if [ "$RF_MODE" = survey ]; then
        [ -n "$(find "$real" -mindepth 1 -maxdepth 1 -print 2>/dev/null | head -n 1)" ] || return
        row RFC "$kind" "" "$real" "$(kib "$real")" "" ""
        return
      fi
      [ "$real" = "$expected" ] || { refuse "DerivedData resolves to $real, not the reviewed $expected"; return; }
      size="$(kib "$real")"
      if empty_dir "$real"; then row RFX "$kind" "" "$real" "$size" "" ""; else refuse "couldn't remove everything inside $real"; fi
      ;;
    simulators)
      command -v xcrun >/dev/null 2>&1 || { [ "$RF_MODE" = survey ] || refuse "xcrun isn't available"; return; }
      local count
      count="$(xcrun simctl list devices unavailable 2>/dev/null | grep -c '(' || true)"
      if [ "$RF_MODE" = survey ]; then
        [ "${count:-0}" -gt 0 ] 2>/dev/null || return
        row RFC "$kind" "" "" "" "$count" ""
        return
      fi
      if xcrun simctl delete unavailable >/dev/null 2>&1; then row RFX "$kind" "" "" "" "$count" ""; else refuse "xcrun simctl delete unavailable failed"; fi
      ;;
    docker)
      if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
        [ "$RF_MODE" = survey ] || refuse "Docker isn't available"
        return
      fi
      if [ "$RF_MODE" = survey ]; then
        local detail
        detail="$(docker system df --format '{{.Type}}: {{.Reclaimable}}' 2>/dev/null | tr '\n' ';' | sed 's/;$//; s/;/; /g')"
        row RFC "$kind" "" "" "" "" "$detail"
        return
      fi
      local out
      if out="$(docker system prune -f 2>&1)"; then
        row RFX "$kind" "" "" "" "" "$(printf '%s\n' "$out" | tail -n 1)"
      else
        refuse "docker system prune -f failed: $(printf '%s\n' "$out" | tail -n 1)"
      fi
      ;;
    *) refuse "unknown cleanup target $kind" ;;
  esac
}
