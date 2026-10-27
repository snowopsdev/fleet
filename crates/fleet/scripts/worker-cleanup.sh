#!/usr/bin/env bash
# Disk cleanup on a macOS or Linux worker. The controller sets RF_MODE
# (survey: read-only, or clean), RF_DIAG_DAYS and RF_SECONDS (this script's
# own deadline), then calls
#   runner_target KIND RUNNER ROOT EXPECTED [NAME...]
#   machine_target KIND EXPECTED
# once per target, with the runner's service already stopped: it refuses
# otherwise, and while a job or a self-update runs. Every path is derived here
# from a fixed suffix of the exact runner directory (or the home directory)
# and must resolve to exactly that place: no symlink below it, nothing outside
# it, no globs. Directory contents are removed with find -delete, which never
# follows links or crosses filesystems. .runner, .credentials*, config files
# and the bin and externals the runner uses are never targets; only previous
# versions of them (bin.VERSION and externals.VERSION that bin and externals
# don't point at) are, by exact reviewed name.
#
# While it removes anything in a runner directory it holds
# RUNNER/.fleet-maintenance (the PIDs doing the work), which the start,
# restart and upgrade scripts refuse while any of them is alive. It ignores
# SIGHUP so a dropped connection doesn't leave a half-finished removal
# without its lock, and stops starting new work at its deadline.
# Rows: RFC (survey), RFF (survey: a named entry of a target), RFX (removed),
# RFE (refused or failed).
set -uo pipefail
set -f
trap '' HUP

: "${RF_MODE:?}"
: "${RF_DIAG_DAYS:?}"
: "${RF_SECONDS:=840}"
case "$RF_MODE" in survey|clean) ;; *) echo "invalid RF_MODE" >&2; exit 2 ;; esac
case "$RF_DIAG_DAYS" in ''|*[!0-9]*) echo "invalid RF_DIAG_DAYS" >&2; exit 2 ;; esac
case "$RF_SECONDS" in ''|*[!0-9]*) echo "invalid RF_SECONDS" >&2; exit 2 ;; esac
deadline=$(( $(date +%s) + RF_SECONDS ))

tab="$(printf '\t')"
row() { local IFS="$tab"; printf '%s\n' "$*"; }

# Command lines of every process, read in full before searching so the search
# itself (whose arguments contain the pattern) isn't in the list.
processes() { ps -A -o command= 2>/dev/null; }
# A process of NAME from ROOT's bin, or a versioned bin.VERSION after a
# self-update (the listener resolves the bin link to bin.VERSION).
proc_in() {
  local list
  list="$(processes)"
  printf '%s\n' "$list" | grep -F -- "$1/bin" | grep -F -- "$2" >/dev/null
}
# Anything at all running from ROOT, such as its self-update script.
any_from() {
  local list
  list="$(processes)"
  printf '%s\n' "$list" | grep -F -- "$1/" >/dev/null
}
any_named() {
  local list
  list="$(processes)"
  printf '%s\n' "$list" | grep -F -- "$1" >/dev/null
}

# A self-update writes _work/_update.sh and then update.finished when done. A
# recent script without a newer update.finished may still be swapping.
updating() {
  local script="$1/_work/_update.sh"
  [ -f "$script" ] && [ -n "$(find "$script" -mmin -30 -print 2>/dev/null)" ] \
    && ! { [ -f "$1/update.finished" ] && [ "$1/update.finished" -nt "$script" ]; }
}

out_of_time() { [ "$(date +%s)" -ge "$deadline" ]; }

# Runs a removal in the background, adding its PID to the lock, and stops it at the deadline.
bounded() {
  local lock="$1"
  shift
  "$@" &
  local pid=$!
  printf ' %s' "$pid" >> "$lock"
  while kill -0 "$pid" 2>/dev/null; do
    if out_of_time; then
      kill "$pid" 2>/dev/null
      wait "$pid" 2>/dev/null
      return 124
    fi
    sleep 1
  done
  wait "$pid"
}

locks=""
release_locks() {
  local lock
  for lock in $locks; do rm -f -- "$lock"; done
}
trap release_locks EXIT

# Takes RUNNER/.fleet-maintenance for this run, unless something live holds it.
lock_root() {
  local lock="$1/.fleet-maintenance" pid
  case " $locks " in *" $lock "*) return 0 ;; esac
  if [ -f "$lock" ]; then
    for pid in $(cat "$lock" 2>/dev/null); do
      case "$pid" in ''|*[!0-9]*) continue ;; esac
      if kill -0 "$pid" 2>/dev/null; then return 1; fi
    done
    rm -f -- "$lock"
  fi
  printf '%s' "$$" > "$lock" || return 1
  locks="$locks $lock"
}

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

# Removes DIR and everything in it. Only for a previous runner version's folder.
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
    previous-bin|previous-externals) echo "" ;;
    *) return 1 ;;
  esac
}

old_logs() {
  find "$1" -mindepth 1 -maxdepth 1 -type f \( -name 'Runner_*.log' -o -name 'Worker_*.log' \) \
    -mtime "+$RF_DIAG_DAYS" -print | sed 's|.*/||' | sort
}

# The folder bin (or externals) uses: its link's target name, or itself.
in_use() {
  local root="$1" d="$2" link
  if [ -L "$root/$d" ]; then
    link="$(readlink "$root/$d")"
    basename "$link"
  else
    echo "$d"
  fi
}

# Previous versions of bin (or externals): real folders named D.VERSION (or the
# D.fleet-previous an older upgrade kept) that D doesn't point at.
previous_versions() {
  local root="$1" d="$2" current entry name
  [ -f "$root/.fleet-upgrade-swap" ] && return
  current="$(in_use "$root" "$d")"
  find "$root" -mindepth 1 -maxdepth 1 -type d -name "$d.*" -print 2>/dev/null | sort | while IFS= read -r entry; do
    name="${entry##*/}"
    previous_name "$d" "$name" || continue
    [ "$name" = "$current" ] && continue
    printf '%s\n' "$name"
  done
}
previous_name() {
  case "$2" in
    "$1".fleet-previous) return 0 ;;
    "$1".*[!0-9.]*|"$1".) return 1 ;;
    "$1".[0-9]*.[0-9]*.[0-9]*) return 0 ;;
    *) return 1 ;;
  esac
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
  if [ -z "$suffix" ]; then
    real="$real_root"
  else
    real="$(resolve "$real_root" "$suffix")"
    status=$?
    if [ "$status" = 1 ]; then
      [ "$RF_MODE" = survey ] && return
      refuse "$real_root/$suffix no longer exists"; return
    elif [ "$status" != 0 ]; then
      refuse "$real_root/$suffix is a symlink or resolves outside $real_root; left it alone"; return
    fi
  fi
  local d=""
  case "$kind" in previous-bin) d=bin ;; previous-externals) d=externals ;; esac

  if [ "$RF_MODE" = survey ]; then
    local names f count=0 total=0
    case "$kind" in
      diag) names="$(old_logs "$real")" ;;
      previous-*) names="$(previous_versions "$real_root" "$d")" ;;
      *)
        [ -n "$(find "$real" -mindepth 1 -maxdepth 1 -print 2>/dev/null | head -n 1)" ] || return
        row RFC "$kind" "$runner" "$real" "$(kib "$real")" "" ""
        return
        ;;
    esac
    [ -n "$names" ] || return
    while IFS= read -r f; do
      count=$((count + 1))
      total=$((total + $(kib "$real/$f")))
      row RFF "$kind" "$runner" "$f"
    done <<< "$names"
    row RFC "$kind" "$runner" "$real" "$total" "$count" ""
    return
  fi

  if [ "$real" != "$expected" ]; then
    refuse "$real resolves differently from the reviewed $expected; left it alone"; return
  fi
  if proc_in "$real_root" "Runner.Worker"; then
    refuse "a job is running in $real_root; left it alone"; return
  fi
  # The controller stops the service first, so the runner can't take a job mid-removal.
  if proc_in "$real_root" "Runner.Listener"; then
    refuse "$runner's service is still running; it must be stopped before cleaning"; return
  fi
  if any_from "$real_root" || updating "$real_root"; then
    refuse "something is still running from $real_root, or a runner self-update is in progress; left it alone"; return
  fi
  if out_of_time; then
    refuse "the cleanup ran out of time before $real; nothing more was removed"; return
  fi
  if ! lock_root "$real_root"; then
    refuse "another cleanup is still working in $real_root; left it alone"; return
  fi
  local lock="$real_root/.fleet-maintenance"
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
      local f removed=0 freed=0 current
      current="$(in_use "$real_root" "$d")"
      if [ -f "$real_root/.fleet-upgrade-swap" ]; then
        refuse "an upgrade of $runner was interrupted; its previous versions are kept"; return
      fi
      for f in "$@"; do
        if ! previous_name "$d" "$f" || [ "$f" = "$current" ] || [ "$f" = "$d" ]; then
          refuse "$f isn't a previous version of $d here; left it alone"; continue
        fi
        if [ ! -d "$real/$f" ] || [ -L "$real/$f" ] || [ "$(cd "$real/$f" && pwd -P)" != "$real/$f" ]; then
          continue
        fi
        size="$(kib "$real/$f")"
        if bounded "$lock" remove_dir "$real/$f"; then
          removed=$((removed + 1))
          freed=$((freed + size))
        else
          refuse "couldn't remove all of $real/$f"
        fi
      done
      row RFX "$kind" "$runner" "$real" "$freed" "$removed" ""
      ;;
    *)
      size="$(kib "$real")"
      if bounded "$lock" empty_dir "$real"; then
        row RFX "$kind" "$runner" "$real" "$size" "" ""
      else
        refuse "couldn't remove everything inside $real (or ran out of time)"
      fi
      ;;
  esac
}

machine_target() {
  local kind="$1" expected="$2" runner=""
  refuse() { row RFE "$kind" "$runner" "$1"; }
  # Shared caches: no runner of any kind may be running or working on this machine.
  if [ "$RF_MODE" = clean ] && { any_named "Runner.Worker" || any_named "Runner.Listener"; }; then
    refuse "a runner is running on this machine; left machine-wide caches alone"; return
  fi
  if [ "$RF_MODE" = clean ] && out_of_time; then
    refuse "the cleanup ran out of time; machine-wide caches were left alone"; return
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
      local lock
      lock="$(mktemp "${TMPDIR:-/tmp}/fleet-cleanup.XXXXXX")"
      if bounded "$lock" empty_dir "$real"; then row RFX "$kind" "" "$real" "$size" "" ""; else refuse "couldn't remove everything inside $real (or ran out of time)"; fi
      rm -f -- "$lock"
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
