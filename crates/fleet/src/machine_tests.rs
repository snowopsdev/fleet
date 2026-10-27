use super::*;
use crate::desired::process::{CommandError, CommandResult, CommandRunner, NonZeroExit};
use std::sync::Mutex;

fn inventory_host(id: &str) -> Host {
    crate::demo_desired::inventory().find_host(id).unwrap().clone()
}

fn host(id: &str) -> MachineTarget {
    MachineTarget::Inventory(inventory_host(id))
}

const MAC_HEALTH: &str = "RFM\tos\tmacos\nRFM\tos_version\t15.6\nRFM\tcores\t12\nRFM\tcpu\t23.50\nRFM\tload\t1.5 2.0 0.8\nRFM\tmemory_total\t24000000000\nRFM\tmemory_available\t8000000000\nRFM\tuptime\t86400\nRFM\tdisk\t/\t500000000000\t300000000000\nRFM_END\nRFM_RUNNERS\n";
const MAC_RUNNERS: &str = "RF1\tmini-02\t/Users/ci/gh-runners/acme-demo/runner-01\ttrue\tactive\t2.329.0\n";

#[test]
fn parses_health_and_exact_runner_facts() {
    let report = parse_inspection(
        &host("mac-mini-02"),
        &format!("login banner\n{MAC_HEALTH}{MAC_RUNNERS}"),
    )
    .unwrap();
    assert_eq!(
        report,
        MachineReport {
            os: OsFamily::Macos,
            os_version: Some("15.6".into()),
            cpu_percent: Some(23.5),
            load_average: Some([1.5, 2.0, 0.8]),
            logical_cores: Some(12),
            memory_total_bytes: Some(24_000_000_000),
            memory_available_bytes: Some(8_000_000_000),
            uptime_secs: Some(86400),
            boot_id: None,
            disks: vec![DiskStats {
                mount_point: "/".into(),
                total_bytes: 500_000_000_000,
                available_bytes: 300_000_000_000,
                runner_root: false,
            }],
            runners: [(
                "mini-02".into(),
                LocalRunner::new(
                    "mini-02",
                    "/Users/ci/gh-runners/acme-demo/runner-01",
                    true,
                    "active",
                    Some("2.329.0")
                )
            )]
            .into(),
            warnings: vec![],
            diagnostics: BTreeMap::new(),
        }
    );
}

#[test]
fn unavailable_metrics_stay_unknown_but_runner_data_survives() {
    let output = format!("RFM\tos\tmacos\nRFM\tcpu\t\nRFM_END\nRFM_RUNNERS\n{MAC_RUNNERS}");
    let report = parse_inspection(&host("mac-mini-02"), &output).unwrap();
    assert_eq!(
        (
            report.cpu_percent,
            report.memory_total_bytes,
            report.memory_available_bytes
        ),
        (None, None, None)
    );
    assert!(report.runners["mini-02"].configured);
    assert!(!report.warnings.is_empty());
}

#[test]
fn boot_session_identity_survives_health_protocol_parsing() {
    for boot_id in ["boot-session-uuid", "2026-10-08T11:00:00.0000000Z"] {
        let output = format!("RFM\tboot_id\t{boot_id}\n{MAC_HEALTH}{MAC_RUNNERS}");
        let report = parse_inspection(&host("mac-mini-02"), &output).unwrap();
        assert_eq!(report.boot_id.as_deref(), Some(boot_id));
    }
}

#[test]
fn machines_without_inventory_runners_can_report_health() {
    for (id, os, runner_output) in [("mac-mini-02", "macos", ""), ("win-builder", "windows", "[]")] {
        let mut host = inventory_host(id);
        host.runners.clear();
        let report = parse_inspection(
            &MachineTarget::Inventory(host),
            &format!("RFM\tos\t{os}\nRFM_END\nRFM_RUNNERS\n{runner_output}"),
        )
        .unwrap();
        assert!(report.runners.is_empty());
    }
}

#[test]
fn windows_crlf_json_and_unicode_are_preserved() {
    let output = "RFM\tos\twindows\r\nRFM\tos_version\tWindows 11 Pro\r\nRFM\tcpu\t12\r\nRFM\tmemory_total\t32000000000\r\nRFM\tmemory_available\t16000000000\r\nRFM\tdisk\tC:\\\t512000000000\t140000000000\r\nRFM_END\r\nRFM_RUNNERS\r\n[{\"name\":\"win-builder\",\"path\":\"C:\\\\runners\\\\équipe\",\"configured\":true,\"service\":\"running\",\"version\":\"2.329.0\"}]\r\n";
    let report = parse_inspection(&host("win-builder"), output).unwrap();
    assert_eq!(report.os, OsFamily::Windows);
    assert_eq!(report.runners["win-builder"].path, "C:\\runners\\équipe");
    assert_eq!(report.disks[0].mount_point, "C:\\");
}

#[test]
fn linux_and_missing_runner_configuration_are_readable() {
    let mut linux = inventory_host("build-linux-01");
    linux.runners.truncate(1);
    let linux = MachineTarget::Inventory(linux);
    let output = format!(
        "{}RF1\tlinux-01-a\t/home/builder/runners/a\tfalse\tmissing\t\n",
        MAC_HEALTH.replace("macos", "linux")
    );
    let report = parse_inspection(&linux, &output).unwrap();
    assert_eq!(
        report.runners["linux-01-a"],
        LocalRunner::new("linux-01-a", "/home/builder/runners/a", false, "missing", None)
    );
}

#[test]
fn partial_or_invalid_responses_are_errors_instead_of_healthy_zeros() {
    let complete = format!("{MAC_HEALTH}{MAC_RUNNERS}");
    for output in [
        complete.replace("RFM_END\n", ""),
        complete.replace("RFM_RUNNERS\n", ""),
        complete.replace("23.50", "NaN"),
        complete.replace("23.50", "101"),
        complete.replace("8000000000", "25000000000"),
        complete.replace("1.5 2.0 0.8", "1.5"),
        complete.replace("macos", "windows"),
        complete.replace(MAC_RUNNERS, ""),
        complete.replace("mini-02", "other-runner"),
    ] {
        assert!(
            parse_inspection(&host("mac-mini-02"), &output).is_err(),
            "accepted {output:?}"
        );
    }
}

#[derive(Default)]
struct FakeCommands(Mutex<Vec<(Vec<String>, String)>>);

impl CommandRunner for FakeCommands {
    fn run(
        &self,
        command: &[String],
        input: Option<&str>,
        _: Duration,
        _: NonZeroExit,
    ) -> Result<CommandResult, CommandError> {
        self.0.lock().unwrap().push((command.to_vec(), input.unwrap().into()));
        Ok(CommandResult::ok(format!("{MAC_HEALTH}{MAC_RUNNERS}")))
    }
}

#[test]
fn one_ssh_session_delivers_health_and_runner_checks_on_stdin() {
    let fake = Arc::new(FakeCommands::default());
    let remote = RemoteClient::new(fake.clone(), Duration::from_secs(30), Programs::default());
    let host = host("mac-mini-02");
    let report = remote.inspect(&host).unwrap();
    assert!(report.runners["mini-02"].configured);
    let calls = fake.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (argv, stdin) = &calls[0];
    assert_eq!(argv.last().unwrap(), "bash -s");
    assert!(argv.contains(&"StrictHostKeyChecking=yes".into()));
    assert!(argv.contains(&"ForwardAgent=no".into()));
    assert!(stdin.contains("/proc/meminfo"));
    assert!(stdin.contains("$HOME/gh-runners/acme-demo/runner-01") || stdin.contains("gh-runners/acme-demo/runner-01"));
}

#[test]
fn runner_root_filesystem_is_marked_and_shares_an_entry_with_the_same_mount() {
    let disk = |mount: &str, free: u64, runner_root: bool| DiskStats {
        mount_point: mount.into(),
        total_bytes: 1000,
        available_bytes: free,
        runner_root,
    };
    let health = MAC_HEALTH.replace(
        "RFM\tdisk\t/\t500000000000\t300000000000\n",
        "RFM\tdisk\t/\t1000\t400\nRFM\tdisk\t/System/Volumes/Data\t1000\t400\n",
    );
    for (root_row, expected) in [
        (
            "RFM\troot_disk\t/System/Volumes/Data\t1000\t400\n",
            vec![disk("/", 400, false), disk("/System/Volumes/Data", 400, true)],
        ),
        (
            "RFM\troot_disk\t/Volumes/CI SSD\t1000\t50\n",
            vec![
                disk("/", 400, false),
                disk("/System/Volumes/Data", 400, false),
                disk("/Volumes/CI SSD", 50, true),
            ],
        ),
        (
            "",
            vec![disk("/", 400, false), disk("/System/Volumes/Data", 400, false)],
        ),
    ] {
        let output = health.replace("RFM_END\n", &format!("{root_row}RFM_END\n")) + MAC_RUNNERS;
        let report = parse_inspection(&host("mac-mini-02"), &output).unwrap();
        assert_eq!(report.disks, expected);
    }
    for bad in [
        "RFM\troot_disk\t/x\t0\t0\n",
        "RFM\troot_disk\t/x\t10\t11\n",
        "RFM\troot_disk\t\t10\t1\n",
    ] {
        let output = format!("{bad}{MAC_HEALTH}{MAC_RUNNERS}");
        assert!(
            parse_inspection(&host("mac-mini-02"), &output).is_err(),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn the_runner_root_travels_quoted_in_the_script_for_each_shell() {
    let mac = inspection_script(&host("mac-mini-02"));
    assert!(
        mac.starts_with("RFM_RUNNER_ROOT=\"$HOME\"/gh-runners/acme-demo\n"),
        "{mac}"
    );
    let windows = inspection_script(&host("win-builder"));
    assert!(
        windows.starts_with("$RfmRunnerRoot = 'C:\\gh-runners\\acme-demo'\n"),
        "{windows}"
    );
    let discovered = MachineTarget::Discovered(DiscoveredMachine {
        id: "ts:n1".into(),
        address: "n1".into(),
        os: OsFamily::Linux,
        ssh_transport: SshTransport::Openssh,
    });
    assert!(inspection_script(&discovered).starts_with(POSIX_HEALTH));
}

/// Runs the POSIX health part of the inspection script with `HOME` in a temporary directory.
#[cfg(unix)]
fn run_posix_health(target: &MachineTarget, home: &std::path::Path) -> String {
    let script = inspection_script(target);
    let health = script.split("\n(\n").next().unwrap();
    let output = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(health)
        .env("HOME", home)
        .current_dir(home)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn posix_health_reports_the_filesystem_of_a_root_that_does_not_exist_yet() {
    let home = tempfile::tempdir().unwrap();
    let output = run_posix_health(&host("mac-mini-02"), home.path());
    let row = output
        .lines()
        .find(|l| l.starts_with("RFM\troot_disk\t"))
        .unwrap_or_else(|| panic!("no root_disk row in {output}"));
    // `HOME` is reported too, and the missing root falls back to it.
    let mount = |row: &str| row.split('\t').nth(2).map(str::to_string);
    let disks: Vec<_> = output
        .lines()
        .filter(|l| l.starts_with("RFM\tdisk\t"))
        .filter_map(mount)
        .collect();
    assert!(disks.contains(&mount(row).unwrap()), "{output}");
}

#[cfg(unix)]
#[test]
fn a_hostile_root_is_never_evaluated_by_the_remote_shell() {
    let home = tempfile::tempdir().unwrap();
    let mut hostile = inventory_host("mac-mini-02");
    // Inventory validation rejects this; the script must stay inert even so.
    hostile.root = "~/gh-runners/x'$(touch pwned)'\"$(touch pwned2)\"`touch pwned3`".into();
    run_posix_health(&MachineTarget::Inventory(hostile), home.path());
    for name in ["pwned", "pwned2", "pwned3"] {
        assert!(!home.path().join(name).exists(), "{name} was created");
    }
}

/// Fails the first run with `first_error`, then answers like `FakeCommands`.
struct FlakyMux {
    calls: Mutex<Vec<Vec<String>>>,
    first_error: &'static str,
}

impl CommandRunner for FlakyMux {
    fn run(
        &self,
        command: &[String],
        _: Option<&str>,
        _: Duration,
        _: NonZeroExit,
    ) -> Result<CommandResult, CommandError> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(command.to_vec());
        if calls.len() == 1 {
            return Err(CommandError::new(command, 255, self.first_error));
        }
        Ok(CommandResult::ok(format!("{MAC_HEALTH}{MAC_RUNNERS}")))
    }
}

#[cfg(unix)]
#[test]
fn shared_connections_keep_hardening_and_retry_a_broken_master_without_sharing() {
    let sockets = tempfile::Builder::new().prefix("rd").tempdir_in("/tmp").unwrap();
    let control = crate::ssh_reuse::ControlDir::prepare(&sockets.path().join("ssh")).unwrap();
    let fake = Arc::new(FlakyMux {
        calls: Mutex::default(),
        first_error: "mux_client_request_session: read from master failed: Broken pipe",
    });
    let remote = RemoteClient::new(fake.clone(), Duration::from_secs(30), Programs::default());
    remote.inspect_shared(&host("mac-mini-02"), &control).unwrap();
    let calls = fake.calls.lock().unwrap();
    let shared = &calls[0];
    let target = shared.iter().position(|a| a == "--").unwrap();
    let options = &shared[1..target];
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ForwardAgent=no",
        "ClearAllForwardings=yes",
        "ControlMaster=auto",
        "ControlPersist=120",
    ] {
        assert!(options.iter().any(|o| o == option), "{option} missing from {shared:?}");
    }
    let path = format!("ControlPath={}/%C", control.path().display());
    assert!(options.contains(&path), "{shared:?}");
    assert_eq!(&shared[target..], ["--", "mac-mini-02", "bash -s"]);
    // The retry is an ordinary, unshared connection.
    assert!(calls[1].iter().all(|a| !a.starts_with("Control")), "{:?}", calls[1]);
    assert_eq!(calls.len(), 2);
}

#[cfg(unix)]
#[test]
fn machine_failures_are_not_retried_and_tailscale_ssh_is_never_shared() {
    let sockets = tempfile::Builder::new().prefix("rd").tempdir_in("/tmp").unwrap();
    let control = crate::ssh_reuse::ControlDir::prepare(&sockets.path().join("ssh")).unwrap();
    let fake = Arc::new(FlakyMux {
        calls: Mutex::default(),
        first_error: "ssh: connect to host mac-mini-02 port 22: Operation timed out",
    });
    let remote = RemoteClient::new(fake.clone(), Duration::from_secs(30), Programs::default());
    assert!(remote.inspect_shared(&host("mac-mini-02"), &control).is_err());
    assert_eq!(fake.calls.lock().unwrap().len(), 1);

    // The demo Linux host uses `tailscale ssh`, which takes no OpenSSH options.
    let fake = Arc::new(FakeCommands::default());
    let remote = RemoteClient::new(fake.clone(), Duration::from_secs(30), Programs::default());
    // The fake answers as a Mac, so this read fails after the command is recorded.
    let _ = remote.inspect_shared(&host("build-linux-01"), &control);
    let calls = fake.0.lock().unwrap();
    assert_eq!(calls[0].0[..2], ["tailscale".to_string(), "ssh".into()]);
    assert!(calls[0].0.iter().all(|a| !a.contains("Control")), "{:?}", calls[0].0);
}
