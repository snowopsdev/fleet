use super::*;
use crate::desired::model::SshTransport;
use crate::machine::{DiscoveredMachine, MachineTarget, parse_inspection};

/// A machine whose reported OS is accepted as is.
fn any_os() -> MachineTarget {
    MachineTarget::Discovered(DiscoveredMachine {
        id: "ts:test".into(),
        address: "test".into(),
        os: OsFamily::Other,
        ssh_transport: SshTransport::Openssh,
    })
}

fn report(os: &str, facts: &[(&str, &str)]) -> MachineReport {
    let mut report = parse_inspection(&any_os(), &format!("RFM\tos\t{os}\nRFM_END\nRFM_RUNNERS\n")).unwrap();
    report.diagnostics = facts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    report
}

fn check<'a>(checks: &'a [MachineDiagnostic], id: &str) -> &'a MachineDiagnostic {
    checks.iter().find(|c| c.id == id).unwrap()
}

#[test]
fn sleep_ssh_and_startup_distinguish_unknown_disabled_and_unsupported() {
    let mac = evaluate(
        &report(
            "macos",
            &[
                ("tailscale_install", "mac-app-store"),
                ("tailscale_ssh", "false"),
                ("ac_sleep", "0"),
            ],
        ),
        None,
    );
    assert_eq!(
        (
            check(&mac, "tailscale-ssh").status,
            check(&mac, "tailscale-ssh").value.as_str()
        ),
        (DiagnosticStatus::Info, "Not supported")
    );
    assert_eq!(check(&mac, "power-sleep").status, DiagnosticStatus::Pass);
    let unknown = evaluate(&report("macos", &[("ac_sleep", "oops")]), None);
    for id in ["tailscale-install", "tailscale-ssh", "power-sleep"] {
        assert_eq!(check(&unknown, id).status, DiagnosticStatus::Unknown);
    }
    let windows = evaluate(
        &report(
            "windows",
            &[
                ("tailscale_unattended", "false"),
                ("ac_sleep", "900"),
                ("ac_displaysleep", "60"),
            ],
        ),
        None,
    );
    assert_eq!(check(&windows, "tailscale-ssh").value, "Not supported");
    assert_eq!(check(&windows, "tailscale-unattended").status, DiagnosticStatus::Warn);
    assert_eq!(check(&windows, "power-sleep").status, DiagnosticStatus::Warn);
    assert_eq!(check(&windows, "power-displaysleep").status, DiagnosticStatus::Info);
}

#[test]
fn unmasked_linux_targets_do_not_claim_the_machine_will_sleep() {
    let linux = evaluate(
        &report("linux", &[("target_sleep", "static"), ("idle_action", "ignore")]),
        None,
    );
    assert_eq!(check(&linux, "power-sleep").status, DiagnosticStatus::Unknown);
    assert_eq!(check(&linux, "power-idle").status, DiagnosticStatus::Pass);
    let masked = evaluate(
        &report("linux", &[("target_sleep", "masked"), ("idle_action", "suspend")]),
        None,
    );
    assert_eq!(check(&masked, "power-sleep").status, DiagnosticStatus::Pass);
    assert_eq!(check(&masked, "power-idle").status, DiagnosticStatus::Info);
}

#[test]
fn runner_checks_use_actual_services_and_exclude_decommissioned_runners() {
    let inventory = crate::demo_desired::inventory();
    let mut host = inventory.find_host("mac-mini-02").unwrap().clone();
    let mut report = report("macos", &[]);
    report.runners.insert(
        host.runners[0].name.clone(),
        crate::desired::model::LocalRunner::new(&host.runners[0].name, "/runners/a", true, "inactive", Some("2.329.0")),
    );
    let checks = evaluate(&report, Some(&host));
    assert_eq!(
        check(&checks, &format!("runner:{}", host.runners[0].name)).status,
        DiagnosticStatus::Warn
    );
    host.runners[0].decommission = true;
    assert!(
        evaluate(&report, Some(&host))
            .iter()
            .all(|c| c.group != DiagnosticGroup::Runners)
    );
    assert!(
        evaluate(&report, None)
            .iter()
            .all(|c| c.group != DiagnosticGroup::Runners)
    );
}

#[test]
fn incomplete_diagnostics_do_not_replace_unknown_with_partial_success() {
    for (end, expected) in [("", false), ("RFM\tdiag:complete\t1\n", true)] {
        let report = parse_inspection(
            &any_os(),
            &format!("RFM\tos\tlinux\nRFM_END\nRFM\tdiag:tailscale_ssh\ttrue\n{end}RFM_RUNNERS\n"),
        )
        .unwrap();
        assert_eq!(report.diagnostics.contains_key("tailscale_ssh"), expected);
    }
}

/// Fake native commands for the POSIX diagnostics script. `TEST_HANG=security` makes the
/// keychain check hang, as a stuck securityd would.
#[cfg(unix)]
const FAKE_COMMANDS: &str = r#"#!/bin/sh
case "${0##*/}" in
  uname) if [ "$1" = -s ]; then printf '%s\n' "$TEST_OS"; else echo arm64; fi ;;
  id) echo builder ;;
  ps) echo /usr/local/bin/tailscaled ;;
  stat) echo builder ;;
  getconf) echo "${TEST_BITS:-64}" ;;
  dpkg) echo armhf ;;
  pmset) printf 'Battery Power:\n sleep 5\nAC Power:\n sleep 0\n displaysleep 10\n autorestart 1\n womp 0\n' ;;
  systemctl) case "$2" in tailscaled.service) echo enabled ;; *) echo masked ;; esac ;;
  systemd-analyze) printf '[Login]\nIdleAction=ignore\n# override\nIdleAction=suspend\n' ;;
  defaults) echo builder ;;
  ioreg) printf '%s\n' '+-o Root  <class IORegistryEntry>' \
    '      "IOConsoleUsers" = ({"kCGSSessionUserNameKey"="other","kCGSSessionOnConsoleKey"=No},{"kCGSSessionOnConsoleKey"=Yes,"kCGSSessionUserNameKey"="builder","CGSSessionScreenIsLocked"=Yes,"kCGSSessionLongUserNameKey"="Build User"})' ;;
  security) if [ "$1" = find-identity ]; then
    printf '  1) 0123456789ABCDEF "Apple Development: SECRET_IDENTITY"\n     1 valid identities found\n'
  elif [ "${TEST_HANG-}" = security ]; then
    exec sleep 47
  else
    echo 'security: SecKeychainCopySettings: User interaction is not allowed.' >&2; exit 36
  fi ;;
  xcode-select) echo /Library/Developer/CommandLineTools ;;
  PlistBuddy) case "$2" in *Short*) cat "$3" ;; *) echo 17A100 ;; esac ;;
  xcodebuild) echo 'You have not agreed to the Xcode license agreements.' >&2; exit 69 ;;
  tailscale) if [ "$1" = debug ]; then
    printf '{\n  "RunSSH": true,\n  "Persist": {"PrivateNodeKey": "DO_NOT_EXPORT"}\n}\n'
  else
    printf '{\n  "Version": "1.90.0",\n  "BackendState": "Running"\n}\n'
  fi ;;
esac
"#;

/// A directory of fake commands, a home with a login keychain and a fake Xcode bundle.
#[cfg(unix)]
fn fake_machine() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let executable = |path: &std::path::Path| {
        std::fs::write(path, FAKE_COMMANDS).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    for name in [
        "uname",
        "id",
        "ps",
        "stat",
        "getconf",
        "dpkg",
        "pmset",
        "systemctl",
        "systemd-analyze",
        "tailscale",
        "defaults",
        "ioreg",
        "security",
        "xcode-select",
        "PlistBuddy",
    ] {
        executable(&dir.path().join(name));
    }
    let keychains = dir.path().join("home/Library/Keychains");
    std::fs::create_dir_all(&keychains).unwrap();
    std::fs::write(keychains.join("login.keychain-db"), "").unwrap();
    let app = dir.path().join("Applications/Xcode-26.1.app/Contents");
    std::fs::create_dir_all(app.join("Developer/usr/bin")).unwrap();
    std::fs::write(app.join("version.plist"), "26.1").unwrap();
    executable(&app.join("Developer/usr/bin/xcodebuild"));
    dir
}

/// Runs the diagnostics script with `shell` against `fake_machine` and parses its facts.
#[cfg(unix)]
fn run_diagnostics(shell: &str, dir: &tempfile::TempDir, os: &str, env: &[(&str, &str)]) -> (String, MachineReport) {
    let output = std::process::Command::new(shell)
        .arg("-c")
        .arg(include_str!("../scripts/machine-diagnostics.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .env("TEST_OS", os)
        .env("HOME", dir.path().join("home"))
        .env("RFM_APPLICATIONS", dir.path().join("Applications"))
        .env("RFM_PLISTBUDDY", dir.path().join("PlistBuddy"))
        .envs(env.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{shell}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    let report = parse_inspection(&any_os(), &format!("RFM\tos\tother\nRFM_END\n{output}RFM_RUNNERS\n")).unwrap();
    (output, report)
}

/// Production runs the script with `bash -s` (bash 3.2 on macOS); `sh` keeps it portable.
#[cfg(unix)]
const SHELLS: [&str; 2] = ["/bin/bash", "/bin/sh"];

#[cfg(unix)]
#[test]
fn posix_collection_reads_fake_macos_and_linux_without_emitting_private_preferences() {
    let dir = fake_machine();
    for shell in SHELLS {
        for os in ["Darwin", "Linux"] {
            let (output, report) = run_diagnostics(shell, &dir, os, &[]);
            assert!(!output.contains("DO_NOT_EXPORT"));
            assert!(!output.contains("SECRET_IDENTITY"));
            assert!(!output.contains("Build User"));
            assert_eq!(report.diagnostics["tailscale_ssh"], "true");
            assert_eq!(report.diagnostics["tailscale_version"], "1.90.0");
            assert_eq!(report.diagnostics["identity"], "builder");
            assert_eq!(report.diagnostics["userland_bits"], "64");
            if os == "Darwin" {
                assert_eq!(report.diagnostics["ac_sleep"], "0");
                assert_eq!(report.diagnostics["battery_sleep"], "5");
                let xcode = format!("26.1|17A100|{}/Applications/Xcode-26.1.app", dir.path().display());
                for (key, value) in [
                    ("auto_login_status", "0"),
                    ("auto_login_user", "builder"),
                    ("console_session_user", "builder"),
                    ("screen_locked", "yes"),
                    ("login_keychain", "36"),
                    ("codesign_identities", "1"),
                    ("developer_dir", "/Library/Developer/CommandLineTools"),
                    ("xcode_apps", xcode.as_str()),
                    ("xcode_license", "69"),
                ] {
                    assert_eq!(
                        report.diagnostics.get(key).map(String::as_str),
                        Some(value),
                        "{shell} {key}"
                    );
                }
            } else {
                assert_eq!(report.diagnostics["target_sleep"], "masked");
                assert_eq!(report.diagnostics["idle_action"], "suspend");
                assert_eq!(report.diagnostics["dpkg_architecture"], "armhf");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn a_hanging_check_spends_only_the_readiness_budget_and_later_checks_stay_unknown() {
    let dir = fake_machine();
    for shell in SHELLS {
        let started = std::time::Instant::now();
        let (_, mut report) = run_diagnostics(
            shell,
            &dir,
            "Darwin",
            &[("TEST_HANG", "security"), ("RFM_READINESS_SECONDS", "2")],
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{shell}: {:?}",
            started.elapsed()
        );
        // The stuck keychain check uses up the budget and ends there; later checks are skipped.
        assert_eq!(report.diagnostics["auto_login_status"], "0");
        assert_eq!(report.diagnostics["login_keychain"], "137", "{shell}");
        for key in ["codesign_identities", "developer_dir", "xcode_scan", "xcode_license"] {
            let value = report.diagnostics.get(key).filter(|v| !v.is_empty());
            assert_eq!(value, None, "{shell} {key}");
        }
        report.os = OsFamily::Macos;
        let checks = evaluate(&report, Some(&mac_host()));
        for id in ["login-keychain", "codesign-identities", "xcode-installed"] {
            assert_eq!(check(&checks, id).status, DiagnosticStatus::Unknown, "{shell} {id}");
        }
        assert!(
            check(&checks, "login-keychain")
                .detail
                .contains("didn't finish in time")
        );
        // Health and runner facts outside the readiness block are unaffected.
        assert_eq!(report.diagnostics["ac_sleep"], "0");
    }
    let (_, mut skipped) = run_diagnostics("/bin/bash", &dir, "Darwin", &[("RFM_READINESS_SECONDS", "0")]);
    skipped.os = OsFamily::Macos;
    assert_eq!(skipped.diagnostics["auto_login_status"], "124");
    let auto = evaluate(&skipped, Some(&mac_host()));
    let auto = check(&auto, "auto-login");
    assert_eq!(auto.status, DiagnosticStatus::Unknown);
    assert!(auto.detail.contains("ran out of time"), "{}", auto.detail);
}

/// The `bounded` helper from the diagnostics script, by itself.
#[cfg(unix)]
fn bounded_function() -> &'static str {
    let script = include_str!("../scripts/machine-diagnostics.sh");
    let start = script.find("bounded() {").unwrap();
    let end = start + script[start..].find("\n}\n").unwrap() + 3;
    &script[start..end]
}

/// Whether a process with exactly this command line is still running.
#[cfg(unix)]
fn running(command: &str) -> bool {
    let ps = std::process::Command::new("ps")
        .args(["-A", "-o", "args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout).lines().any(|l| l.trim() == command)
}

#[cfg(unix)]
#[test]
fn bounded_kills_a_hanging_command_and_leaves_no_watcher_behind() {
    for shell in SHELLS {
        let started = std::time::Instant::now();
        let output = std::process::Command::new(shell)
            .arg("-c")
            .arg(format!(
                "{}\nout=\"$(bounded 1 sleep 41)\"; echo \"hang=$?\"\nout=\"$(bounded 39 sh -c 'echo hi; exit 3')\"; echo \"fast=$? $out\"\n",
                bounded_function()
            ))
            .output()
            .unwrap();
        let output = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output, "hang=137\nfast=3 hi\n", "{shell}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "{shell}: {:?}",
            started.elapsed()
        );
        // Neither the killed command nor the fast command's watcher outlives the call.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while (running("sleep 41") || running("sleep 39")) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            !running("sleep 41") && !running("sleep 39"),
            "{shell} left processes behind"
        );
    }
}

#[test]
fn runner_volume_is_checked_separately_from_system_disks() {
    let host = crate::demo_desired::inventory()
        .find_host("mac-mini-02")
        .unwrap()
        .clone();
    let disk = |mount: &str, free: u64, runner_root: bool| crate::machine::DiskStats {
        mount_point: mount.into(),
        total_bytes: 1000,
        available_bytes: free,
        runner_root,
    };
    let mut report = report("macos", &[]);
    report.disks = vec![disk("/", 500, false), disk("/Volumes/Builds", 50, true)];
    let checks = evaluate(&report, Some(&host));
    let volume = check(&checks, "runner-disk");
    assert_eq!(
        (volume.status, volume.value.as_str()),
        (DiagnosticStatus::Warn, "5% free")
    );
    assert!(
        volume
            .detail
            .starts_with("/Volumes/Builds holds ~/gh-runners/acme-demo"),
        "{}",
        volume.detail
    );
    assert_eq!(check(&checks, "disk-space").detail, "/: 0.0 GiB free of 0.0 GiB");
    assert!(!check(&evaluate(&report, None), "disk-space").detail.is_empty());
    report.disks = vec![disk("/", 500, true)];
    let checks = evaluate(&report, Some(&host));
    assert_eq!(check(&checks, "runner-disk").status, DiagnosticStatus::Pass);
    // One filesystem isn't reported twice.
    assert!(checks.iter().all(|c| c.id != "disk-space"));
    report.disks = vec![disk("/", 500, false)];
    assert_eq!(
        check(&evaluate(&report, Some(&host)), "runner-disk").status,
        DiagnosticStatus::Unknown
    );
    assert!(evaluate(&report, None).iter().all(|c| c.id != "runner-disk"));
}

#[test]
fn architecture_is_compared_with_the_inventory_platform() {
    use crate::desired::model::Platform;
    let mut host = crate::demo_desired::inventory()
        .find_host("mac-mini-02")
        .unwrap()
        .clone();
    let status = |host: &Host, arch: &str| {
        let report = report("macos", &[("architecture", arch)]);
        check(&evaluate(&report, Some(host)), "architecture").status
    };
    assert_eq!(status(&host, "arm64"), DiagnosticStatus::Pass);
    assert_eq!(status(&host, "x86_64"), DiagnosticStatus::Warn);
    host.platform = Platform::DarwinX64;
    assert_eq!(status(&host, "x86_64"), DiagnosticStatus::Pass);
    assert_eq!(status(&host, "arm64"), DiagnosticStatus::Warn);
    host.platform = Platform::LinuxArm64;
    assert_eq!(status(&host, "aarch64"), DiagnosticStatus::Pass);
    assert_eq!(status(&host, "riscv64"), DiagnosticStatus::Info);
    let discovered = evaluate(&report("linux", &[("architecture", "aarch64")]), None);
    assert_eq!(check(&discovered, "architecture").status, DiagnosticStatus::Info);
}

/// Facts recorded over SSH from a macOS 27 Mac with Xcode 27.1 installed but the Command Line
/// Tools selected and the license not yet accepted (account renamed).
const RECORDED_MAC: &[(&str, &str)] = &[
    ("identity", "builder"),
    ("architecture", "arm64"),
    ("console_user", "builder"),
    ("auto_login_status", "1"),
    ("console_sessions", "1"),
    ("console_session_user", "builder"),
    ("screen_locked", "no"),
    ("login_keychain", "36"),
    ("codesign_identities", "2"),
    ("developer_dir", "/Library/Developer/CommandLineTools"),
    ("xcode_scan", "1"),
    ("xcode_apps", "27.1|27A9275|/Applications/Xcode.app"),
    ("xcode_license_dir", "/Applications/Xcode.app/Contents/Developer"),
    ("xcode_license", "69"),
];

fn summary(checks: &[MachineDiagnostic], ids: &[&str]) -> Vec<(String, DiagnosticStatus, String)> {
    ids.iter()
        .map(|id| {
            let c = check(checks, id);
            (c.id.clone(), c.status, c.value.clone())
        })
        .collect()
}

/// A LaunchAgent build host without GUI lanes or declared toolchains.
fn mac_host() -> Host {
    let mut host = crate::demo_desired::inventory()
        .find_host("mac-mini-02")
        .unwrap()
        .clone();
    host.xcode.clear();
    host.simulator_runtimes.clear();
    host
}

#[test]
fn recorded_mac_readiness_maps_to_clear_findings() {
    let checks = evaluate(&report("macos", RECORDED_MAC), Some(&mac_host()));
    let expected = [
        ("auto-login", DiagnosticStatus::Info, "Off"),
        ("console-session", DiagnosticStatus::Pass, "builder"),
        ("screen-lock", DiagnosticStatus::Pass, "Unlocked"),
        ("login-keychain", DiagnosticStatus::Info, "Locked for SSH sessions"),
        ("codesign-identities", DiagnosticStatus::Info, "2 valid"),
        ("xcode-installed", DiagnosticStatus::Info, "27.1 (27A9275)"),
        ("xcode-selected", DiagnosticStatus::Warn, "Command Line Tools"),
        ("xcode-license", DiagnosticStatus::Warn, "Not accepted"),
    ];
    let ids: Vec<&str> = expected.iter().map(|e| e.0).collect();
    assert_eq!(
        summary(&checks, &ids),
        expected.map(|(id, status, value)| (id.to_string(), status, value.to_string()))
    );
    assert!(
        check(&checks, "xcode-selected")
            .detail
            .contains("sudo xcode-select -s /Applications/Xcode.app")
    );
    assert!(
        checks
            .iter()
            .all(|c| c.id != "xcode-declared" && c.id != "simulator-runtimes")
    );
    // Linux and Windows machines get none of these.
    let linux = evaluate(&report("linux", RECORDED_MAC), None);
    assert!(
        linux
            .iter()
            .all(|c| !matches!(c.group, DiagnosticGroup::Session | DiagnosticGroup::Toolchain))
    );
}

#[test]
fn locked_screens_and_other_console_users_warn_only_where_gui_jobs_run() {
    let mut facts = RECORDED_MAC.to_vec();
    facts.retain(|(k, _)| {
        !matches!(
            *k,
            "screen_locked" | "console_session_user" | "auto_login_user" | "auto_login_status"
        )
    });
    facts.extend([
        ("screen_locked", "yes"),
        ("console_session_user", "admin"),
        ("auto_login_status", "0"),
        ("auto_login_user", "admin"),
    ]);
    let locked = report("macos", &facts);
    let gui = crate::demo_desired::inventory().find_host("studio").unwrap().clone();
    let ids = ["auto-login", "console-session", "screen-lock"];
    assert_eq!(
        summary(&evaluate(&locked, Some(&gui)), &ids),
        [
            (
                "auto-login".to_string(),
                DiagnosticStatus::Warn,
                "Logs in as admin".to_string()
            ),
            ("console-session".into(), DiagnosticStatus::Warn, "admin".into()),
            ("screen-lock".into(), DiagnosticStatus::Warn, "Locked".into()),
        ]
    );
    let builds = evaluate(&locked, Some(&mac_host()));
    assert_eq!(check(&builds, "screen-lock").status, DiagnosticStatus::Info);
    assert_eq!(check(&builds, "console-session").status, DiagnosticStatus::Info);
    let discovered = evaluate(&locked, None);
    assert_eq!(check(&discovered, "auto-login").status, DiagnosticStatus::Info);
    // Nobody on the console: no lock state, and LaunchAgent hosts are told why runners wait.
    facts.retain(|(k, _)| !matches!(*k, "screen_locked" | "console_session_user"));
    let nobody = evaluate(&report("macos", &facts), Some(&mac_host()));
    assert_eq!(check(&nobody, "console-session").status, DiagnosticStatus::Warn);
    assert!(nobody.iter().all(|c| c.id != "screen-lock"));
}

#[test]
fn missing_keychain_timeouts_and_unread_facts_stay_distinct() {
    for (keychain, status, value) in [
        ("0", DiagnosticStatus::Pass, "Unlocked"),
        ("missing", DiagnosticStatus::Warn, "Not found"),
        ("137", DiagnosticStatus::Unknown, "Unknown"),
        ("51", DiagnosticStatus::Unknown, "Unknown"),
    ] {
        let mut facts = RECORDED_MAC.to_vec();
        facts.retain(|(k, _)| *k != "login_keychain");
        facts.push(("login_keychain", keychain));
        let checks = evaluate(&report("macos", &facts), Some(&mac_host()));
        let c = check(&checks, "login-keychain");
        assert_eq!((c.status, c.value.as_str()), (status, value), "{keychain}");
    }
    let unread = evaluate(&report("macos", &[]), Some(&mac_host()));
    for id in [
        "auto-login",
        "console-session",
        "login-keychain",
        "codesign-identities",
        "xcode-installed",
    ] {
        assert_eq!(check(&unread, id).status, DiagnosticStatus::Unknown, "{id}");
    }
    assert!(unread.iter().all(|c| c.id != "xcode-selected"));
}

#[test]
fn declared_xcode_and_simulator_runtimes_are_compared_with_installed_ones() {
    let mut host = mac_host();
    host.xcode = vec!["27".into(), "26.1".into()];
    host.simulator_runtimes = vec!["iOS 27.0".into(), "watchOS 11".into()];
    let mut facts = RECORDED_MAC.to_vec();
    facts.retain(|(k, _)| !matches!(*k, "xcode_license" | "developer_dir"));
    facts.extend([
        ("xcode_license", "0"),
        ("developer_dir", "/Applications/Xcode.app/Contents/Developer"),
        ("simulator_scan", "1"),
        ("simulator_runtimes", "iOS 27.0;tvOS 27.0"),
    ]);
    let checks = evaluate(&report("macos", &facts), Some(&host));
    assert_eq!(
        summary(
            &checks,
            &[
                "xcode-selected",
                "xcode-license",
                "xcode-declared",
                "simulator-runtimes"
            ]
        ),
        [
            (
                "xcode-selected".to_string(),
                DiagnosticStatus::Pass,
                "Xcode 27.1".to_string()
            ),
            ("xcode-license".into(), DiagnosticStatus::Pass, "Accepted".into()),
            ("xcode-declared".into(), DiagnosticStatus::Warn, "Missing 26.1".into()),
            (
                "simulator-runtimes".into(),
                DiagnosticStatus::Warn,
                "Missing watchOS 11".into()
            ),
        ]
    );
    host.xcode = vec!["27.1".into()];
    host.simulator_runtimes = vec!["iOS 27".into()];
    let checks = evaluate(&report("macos", &facts), Some(&host));
    assert_eq!(check(&checks, "xcode-declared").status, DiagnosticStatus::Pass);
    assert_eq!(check(&checks, "simulator-runtimes").status, DiagnosticStatus::Pass);
    // Runtimes can't be listed before the license is accepted.
    let blocked = evaluate(&report("macos", RECORDED_MAC), Some(&host));
    let runtimes = check(&blocked, "simulator-runtimes");
    assert_eq!(runtimes.status, DiagnosticStatus::Unknown);
    assert!(runtimes.detail.contains("license"));
}

#[test]
fn declared_versions_match_by_leading_components() {
    use super::macos::{runtime_satisfies, version_satisfies};
    for (declared, installed, ok) in [
        ("26", "26.1", true),
        ("26.1", "26.1", true),
        ("26.1", "26.1.1", true),
        ("26.0", "26", true),
        ("26.1", "26.10", false),
        ("26.1.1", "26.1", false),
        ("16", "26.1", false),
    ] {
        assert_eq!(version_satisfies(declared, installed), ok, "{declared} vs {installed}");
    }
    assert!(runtime_satisfies("iOS 26.0", "iOS 26.0.1"));
    assert!(runtime_satisfies("visionOS 26", "visionOS 26.0"));
    assert!(runtime_satisfies("xrOS 1.0", "visionOS 1.0"));
    assert!(!runtime_satisfies("iOS 26.0", "tvOS 26.0"));
    assert!(!runtime_satisfies("iOS 26", "garbage"));
}

#[test]
fn a_32_bit_userland_on_a_64_bit_kernel_fails_the_architecture_check() {
    use crate::desired::model::Platform;
    let mut host = mac_host();
    host.platform = Platform::LinuxArm64;
    for facts in [
        &[("architecture", "aarch64"), ("userland_bits", "32")][..],
        &[
            ("architecture", "aarch64"),
            ("userland_bits", "64"),
            ("dpkg_architecture", "armhf"),
        ],
        &[("architecture", "armv7l")],
    ] {
        let checks = evaluate(&report("linux", facts), Some(&host));
        let arch = check(&checks, "architecture");
        assert_eq!(arch.status, DiagnosticStatus::Warn, "{facts:?}");
        assert!(
            arch.value.ends_with("32-bit userland") && arch.detail.contains("linux-arm64"),
            "{arch:?}"
        );
    }
    let ok = evaluate(
        &report(
            "linux",
            &[
                ("architecture", "aarch64"),
                ("userland_bits", "64"),
                ("dpkg_architecture", "arm64"),
            ],
        ),
        Some(&host),
    );
    assert_eq!(check(&ok, "architecture").status, DiagnosticStatus::Pass);
}
