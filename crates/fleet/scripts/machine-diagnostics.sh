# Read-only host configuration. Run in a subshell so runner probing is unaffected.
export LC_ALL=C
fact() { printf 'RFM\tdiag:%s\t%s\n' "$1" "$(printf '%s' "$2" | tr '\t\r\n' '   ' | cut -c 1-1000)"; }
# Runs a command with stdin closed for at most $1 seconds, so nothing can wait on a prompt.
# macOS has no timeout(1). The watcher's output goes to /dev/null so captures don't wait for it.
bounded() {
  limit="$1"; shift
  "$@" </dev/null &
  bounded_pid=$!
  (
    sleep "$limit" &
    sleeper=$!
    trap 'kill "$sleeper"; exit 0' TERM
    wait "$sleeper" && kill -9 "$bounded_pid"
  ) >/dev/null 2>&1 &
  bounded_watcher=$!
  wait "$bounded_pid"; bounded_status=$?
  kill "$bounded_watcher" >/dev/null 2>&1
  return "$bounded_status"
}
fact identity "$(id -un 2>/dev/null)"
fact architecture "$(uname -m 2>/dev/null)"
os="$(uname -s)"
ts="$(command -v tailscale 2>/dev/null)"
if [ -z "$ts" ]; then
  for candidate in /opt/homebrew/bin/tailscale /usr/local/bin/tailscale /usr/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
    if [ -x "$candidate" ]; then ts="$candidate"; break; fi
  done
fi
fact tailscale_cli "$ts"
if [ -n "$ts" ]; then
  # Read only selected fields. Never emit the complete preferences or state.
  prefs="$(TAILSCALE_BE_CLI=1 "$ts" debug prefs 2>/dev/null)"
  fact tailscale_ssh "$(printf '%s\n' "$prefs" | sed -En 's/^[[:space:]]*"RunSSH":[[:space:]]*(true|false).*/\1/p')"
  status="$(TAILSCALE_BE_CLI=1 "$ts" status --json 2>/dev/null)"
  fact tailscale_version "$(printf '%s\n' "$status" | sed -n 's/^  "Version": "\([^"]*\)".*/\1/p')"
  fact tailscale_backend "$(printf '%s\n' "$status" | sed -n 's/^  "BackendState": "\([^"]*\)".*/\1/p')"
fi
case "$os" in
  Darwin)
    # An installed CLI may be a symlink into the GUI. Check the running daemon too.
    if ps -axo comm= 2>/dev/null | awk '/(^|\/)tailscaled$/ {found=1} END {exit !found}'; then
      fact tailscale_install cli-daemon
    elif [ -d /Applications/Tailscale.app ]; then
      if [ -f /Applications/Tailscale.app/Contents/_MASReceipt/receipt ]; then
        fact tailscale_install mac-app-store
      else
        fact tailscale_install mac-standalone
      fi
    elif [ -n "$ts" ]; then
      fact tailscale_install cli
    fi
    power="$(pmset -g custom 2>/dev/null)"
    for setting in sleep displaysleep autorestart womp; do
      fact "ac_$setting" "$(printf '%s\n' "$power" | awk -v key="$setting" '/^[^[:space:]]/ {ac=($0 ~ /^AC Power:/)} ac && $1==key {print $2; exit}')"
    done
    fact battery_sleep "$(printf '%s\n' "$power" | awk '/^[^[:space:]]/ {battery=($0 ~ /^Battery Power:/)} battery && $1=="sleep" {print $2; exit}')"
    fact console_user "$(stat -f '%Su' /dev/console 2>/dev/null)"
    # The account macOS logs in automatically after a restart (FileVault turns this off).
    if auto="$(bounded 3 defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null)"; then
      fact auto_login_user "$auto"
    fi
    fact auto_login_read 1
    # The session on the console and whether its screen is locked. Only these two fields are kept.
    if registry="$(bounded 3 ioreg -n Root -d1 -k IOConsoleUsers 2>/dev/null)" && [ -n "$registry" ]; then
      session="$(printf '%s\n' "$registry" | awk '/"IOConsoleUsers"/ {
        n = split($0, parts, /\},\{/)
        for (i = 1; i <= n; i++) if (parts[i] ~ /"kCGSSessionOnConsoleKey"=Yes/ && parts[i] ~ /"kCGSSessionUserNameKey"="/) {
          user = parts[i]; sub(/.*"kCGSSessionUserNameKey"="/, "", user); sub(/".*/, "", user)
          print user "|" (parts[i] ~ /"CGSSessionScreenIsLocked"=Yes/ ? "yes" : "no"); exit
        }
      }')"
      fact console_sessions 1
      if [ -n "$session" ]; then
        fact console_session_user "${session%|*}"
        fact screen_locked "${session##*|}"
      fi
    fi
    # Whether this SSH session can read the login keychain without a prompt (exit status only).
    keychain="$HOME/Library/Keychains/login.keychain-db"
    if [ -e "$keychain" ]; then
      bounded 3 security show-keychain-info "$keychain" >/dev/null 2>&1
      fact login_keychain "$?"
    else
      fact login_keychain missing
    fi
    # Count valid code-signing identities; the identities themselves never leave the machine.
    fact codesign_identities "$(bounded 3 security find-identity -v -p codesigning 2>/dev/null | awk '/valid identities found/ {print $1; exit}')"
    # Installed Xcode versions from each app bundle; never run the /usr/bin shims, which can
    # offer to install the Command Line Tools on the console.
    developer="$(bounded 3 xcode-select -p 2>/dev/null)"
    fact developer_dir "$developer"
    apps=""
    first_xcode=""
    # The RFM_ overrides exist for tests with fake bundles.
    plistbuddy="${RFM_PLISTBUDDY:-/usr/libexec/PlistBuddy}"
    for app in "${RFM_APPLICATIONS:-/Applications}"/Xcode*.app; do
      [ -f "$app/Contents/version.plist" ] || continue
      version="$("$plistbuddy" -c 'Print :CFBundleShortVersionString' "$app/Contents/version.plist" 2>/dev/null)"
      build="$("$plistbuddy" -c 'Print :ProductBuildVersion' "$app/Contents/version.plist" 2>/dev/null)"
      [ -n "$version" ] || continue
      apps="$apps${apps:+;}$version|$build|$app"
      [ -n "$first_xcode" ] || first_xcode="$app/Contents/Developer"
    done
    fact xcode_scan 1
    fact xcode_apps "$apps"
    case "$developer" in
      */Contents/Developer) xcode_dir="$developer" ;;
      *) xcode_dir="$first_xcode" ;;
    esac
    if [ -n "$xcode_dir" ] && [ -x "$xcode_dir/usr/bin/xcodebuild" ]; then
      fact xcode_license_dir "$xcode_dir"
      bounded 5 "$xcode_dir/usr/bin/xcodebuild" -license check >/dev/null 2>&1
      license=$?
      fact xcode_license "$license"
      # Simulator runtimes, only for hosts that declare some (simctl can be slow to start).
      if [ "$license" = 0 ] && [ -n "${RFM_SIMULATOR_RUNTIMES-}" ] && [ -x "$xcode_dir/usr/bin/simctl" ]; then
        if runtimes="$(bounded 8 "$xcode_dir/usr/bin/simctl" list runtimes 2>/dev/null)"; then
          fact simulator_runtimes "$(printf '%s\n' "$runtimes" | awk '/^[A-Za-z]+OS [0-9]/ && !/unavailable/ {
            sub(/ \(.*/, ""); printf "%s%s", sep, $0; sep = ";"
          }')"
          fact simulator_scan 1
        fi
      fi
    fi
    ;;
  Linux)
    if [ -n "$ts" ]; then fact tailscale_install cli-daemon; fi
    if command -v systemctl >/dev/null 2>&1; then
      fact tailscale_startup "$(systemctl is-enabled tailscaled.service 2>/dev/null)"
      for target in sleep suspend hibernate hybrid-sleep suspend-then-hibernate; do
        fact "target_$target" "$(systemctl is-enabled "$target.target" 2>/dev/null)"
      done
    fi
    # cat-config includes drop-ins in precedence order. Ignore is logind's default.
    if logind="$(systemd-analyze cat-config systemd/logind.conf 2>/dev/null)"; then
      fact idle_action "$(printf '%s\n' "$logind" | awk -F= 'BEGIN {action="ignore"} /^[[:space:]]*IdleAction[[:space:]]*=/ {action=$2; gsub(/[[:space:]]/,"",action)} END {print action}')"
    fi
    ;;
esac
fact complete 1
