//! The real cleanup and upgrade scripts, run with bash (and pwsh when it's
//! installed) against throwaway runner directories. No SSH and no network:
//! `curl` is replaced on PATH, and nothing outside the temporary directory is
//! touched.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::super::maintenance::{CleanupKind, CleanupOptions, cleanup_candidates};
use super::super::remote::{FleetRemote as _, clean_script, parse_clean, parse_survey, survey_script};
use super::*;

const UPGRADE_SH: &str = include_str!("../../../scripts/worker-upgrade.sh");
const CLEANUP_PS1: &str = include_str!("../../../scripts/worker-cleanup.ps1");
const UPGRADE_PS1: &str = include_str!("../../../scripts/worker-upgrade.ps1");

/// Runs `script` with bash, extra environment and PATH entries; returns (exit code, stdout, stderr).
fn bash(script: &str, env: &[(&str, &Path)], path_first: Option<&Path>) -> (i32, String, String) {
    let mut command = Command::new("bash");
    command
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(first) = path_first {
        let path = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", first.display()));
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn write_exec(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A process whose command line names `script`, as a runner's own processes do. Killed on drop.
struct Running(Child);

impl Running {
    fn start(script: &Path) -> Self {
        write_exec(script, "#!/bin/sh\nsleep 30\n");
        let child = Command::new("sh").arg(script).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        Self(child)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A configured runner directory for `linux-01` under a temporary root, and the host using it.
fn runner_dir() -> (tempfile::TempDir, PathBuf, Host) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("gh-runners/acme/runner-01");
    for dir in ["_work/_temp/job", "_diag", "bin", "externals"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::write(root.join(".runner"), r#"{"agentName": "linux-01"}"#).unwrap();
    std::fs::write(root.join("_work/_temp/job/file"), "x").unwrap();
    let mut inventory = example_inventory();
    let host = inventory.hosts.iter_mut().find(|h| h.id == "linux").unwrap();
    host.root = base.join("gh-runners/acme").display().to_string();
    let host = host.clone();
    (tmp, root, host)
}

fn clean_temp(host: &Host) -> Vec<super::super::maintenance::CleanupOutcome> {
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(host, &options)
        .into_iter()
        .filter(|c| c.kind == CleanupKind::WorkTemp)
        .collect();
    let survey = parse_survey(&bash(&survey_script(host, &candidates, &options), &[], None).1).unwrap();
    assert_eq!(survey.targets.len(), 1);
    parse_clean(&bash(&clean_script(host, &survey.targets, &options).unwrap(), &[], None).1).unwrap()
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_while_a_job_the_service_or_a_self_update_runs() {
    let (_tmp, root, host) = runner_dir();
    {
        let _job = Running::start(&root.join("bin/Runner.Worker"));
        let outcomes = clean_temp(&host);
        assert!(
            !outcomes[0].ok && outcomes[0].message.contains("a job is running"),
            "{outcomes:?}"
        );
    }
    {
        // After a self-update the listener runs from bin.VERSION, not bin.
        let _service = Running::start(&root.join("bin.2.330.0/Runner.Listener"));
        let outcomes = clean_temp(&host);
        assert!(
            !outcomes[0].ok && outcomes[0].message.contains("still running"),
            "{outcomes:?}"
        );
    }
    {
        let _lock_holder = Running::start(&root.parent().unwrap().join("other/holder.sh"));
        std::fs::write(root.join(".fleet-maintenance"), _lock_holder.0.id().to_string()).unwrap();
        let outcomes = clean_temp(&host);
        assert!(
            !outcomes[0].ok && outcomes[0].message.contains("another cleanup"),
            "{outcomes:?}"
        );
    }
    // A self-update that just wrote its script and hasn't finished.
    std::fs::create_dir_all(root.join("_work/_update")).unwrap();
    std::fs::write(root.join("_work/_update.sh"), "#!/bin/bash\n").unwrap();
    let outcomes = clean_temp(&host);
    assert!(
        !outcomes[0].ok && outcomes[0].message.contains("self-update"),
        "{outcomes:?}"
    );
    assert!(root.join("_work/_temp/job/file").exists(), "nothing was removed");
    // Finished: `_work/_update` stays behind after every self-update, and that's fine.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(root.join("update.finished"), "").unwrap();
    let outcomes = clean_temp(&host);
    assert!(outcomes[0].ok, "{outcomes:?}");
    assert!(!root.join("_work/_temp/job").exists());
    assert!(
        !root.join(".fleet-maintenance").exists(),
        "the stale lock was replaced and released"
    );
}

/// Previous versions after a self-update (bin -> bin.2.330.0): only the folders
/// bin and externals don't point at are offered and removed.
#[cfg(unix)]
#[test]
fn cleanup_offers_only_previous_runner_versions() {
    let (_tmp, root, host) = runner_dir();
    for dir in ["bin", "externals"] {
        std::fs::remove_dir_all(root.join(dir)).unwrap();
        for version in ["2.320.0", "2.330.0"] {
            std::fs::create_dir_all(root.join(format!("{dir}.{version}/x"))).unwrap();
        }
        std::os::unix::fs::symlink(root.join(format!("{dir}.2.330.0")), root.join(dir)).unwrap();
    }
    std::fs::create_dir_all(root.join("bin.not-a-version")).unwrap();
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(&host, &options)
        .into_iter()
        .filter(|c| matches!(c.kind, CleanupKind::PreviousBin | CleanupKind::PreviousExternals))
        .collect();
    let survey = parse_survey(&bash(&survey_script(&host, &candidates, &options), &[], None).1).unwrap();
    let found: Vec<(CleanupKind, Vec<String>)> = survey.targets.iter().map(|t| (t.kind, t.files.clone())).collect();
    assert_eq!(
        found,
        [
            (CleanupKind::PreviousBin, vec!["bin.2.320.0".to_string()]),
            (CleanupKind::PreviousExternals, vec!["externals.2.320.0".to_string()]),
        ]
    );
    // A reviewed list naming the version in use is refused for that entry.
    let mut targets = survey.targets.clone();
    targets[0].files.push("bin.2.330.0".into());
    let outcomes = parse_clean(&bash(&clean_script(&host, &targets, &options).unwrap(), &[], None).1).unwrap();
    assert!(
        outcomes
            .iter()
            .any(|o| !o.ok && o.message.contains("isn't a previous version")),
        "{outcomes:?}"
    );
    assert!(!root.join("bin.2.320.0").exists() && !root.join("externals.2.320.0").exists());
    assert!(root.join("bin.2.330.0/x").exists() && root.join("bin.not-a-version").exists());
}

/// A runner at `$HOME/gh-runners/acme/runner-01` on 2.320.0, a 2.330.0 release
/// served by a fake `curl`, and the upgrade script's input.
struct Upgrade {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    shims: PathBuf,
    checksum: String,
}

fn listener(version: &str) -> String {
    format!("#!/bin/sh\necho {version}\n")
}

impl Upgrade {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        let root = home.join("gh-runners/acme/runner-01");
        write_exec(&root.join("bin/Runner.Listener"), &listener("2.320.0"));
        std::fs::create_dir_all(root.join("externals/node")).unwrap();
        std::fs::write(root.join("externals/node/version"), "old").unwrap();
        std::fs::write(root.join(".runner"), r#"{"agentName": "linux-01"}"#).unwrap();
        std::fs::write(root.join(".credentials"), "secret").unwrap();
        let package = home.join("package");
        write_exec(&package.join("bin/Runner.Listener"), &listener("2.330.0"));
        std::fs::create_dir_all(package.join("externals/node")).unwrap();
        std::fs::write(package.join("externals/node/version"), "new").unwrap();
        std::fs::write(package.join("bin/runsvc.sh"), "new service script").unwrap();
        let archive = home.join("runner.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&package)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success());
        let digest = Command::new("shasum")
            .args(["-a", "256"])
            .arg(&archive)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .or_else(|| Command::new("sha256sum").arg(&archive).output().ok())
            .unwrap();
        let checksum = String::from_utf8_lossy(&digest.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let shims = home.join("shims");
        write_exec(
            &shims.join("curl"),
            &format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = --output ] && cp '{}' \"$2\"; shift; done\n",
                archive.display()
            ),
        );
        Self {
            _tmp: tmp,
            home,
            root,
            shims,
            checksum,
        }
    }

    fn run(&self) -> (i32, String, String) {
        let script = format!(
            "RF_ROOT=\"$HOME\"/gh-runners/acme/runner-01\nRF_NAME=linux-01\n\
RF_ASSET_URL=https://github.com/actions/runner/releases/download/v2.330.0/actions-runner-linux-x64-2.330.0.tar.gz\n\
RF_ASSET_NAME=actions-runner-linux-x64-2.330.0.tar.gz\nRF_CHECKSUM={}\nRF_VERSION=2.330.0\n{UPGRADE_SH}",
            self.checksum
        );
        bash(&script, &[("HOME", &self.home)], Some(&self.shims))
    }

    fn version(&self) -> String {
        let out = Command::new(self.root.join("bin/Runner.Listener")).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Converts the runner to the self-updated layout at `version`: bin and
    /// externals become links to bin.VERSION and externals.VERSION.
    fn versioned(&self, version: &str) {
        for dir in ["bin", "externals"] {
            std::fs::rename(self.root.join(dir), self.root.join(format!("{dir}.{version}"))).unwrap();
            std::os::unix::fs::symlink(self.root.join(format!("{dir}.{version}")), self.root.join(dir)).unwrap();
        }
    }

    /// The folder `name` links to.
    fn link(&self, name: &str) -> String {
        std::fs::read_link(self.root.join(name))
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn externals(&self) -> String {
        std::fs::read_to_string(self.root.join("externals/node/version")).unwrap()
    }

    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[cfg(unix)]
#[test]
fn upgrade_converts_plain_folders_to_the_versioned_layout() {
    let u = Upgrade::new();
    std::fs::write(u.root.join("runsvc.sh"), "old").unwrap();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!((u.version(), u.externals()), ("2.330.0".to_string(), "new".to_string()));
    assert_eq!(
        u.entries(),
        [
            ".credentials",
            ".runner",
            "bin",
            "bin.2.320.0",
            "bin.2.330.0",
            "externals",
            "externals.2.320.0",
            "externals.2.330.0",
            "runsvc.sh"
        ]
    );
    assert_eq!(u.link("bin"), "bin.2.330.0");
    assert_eq!(std::fs::read_to_string(u.root.join(".credentials")).unwrap(), "secret");
    assert_eq!(
        std::fs::read_to_string(u.root.join("runsvc.sh")).unwrap(),
        "new service script"
    );
    // Already current: nothing changes.
    let (code, out, _) = u.run();
    assert_eq!(code, 0);
    assert!(out.contains("already runs 2.330.0"), "{out}");
}

#[cfg(unix)]
#[test]
fn upgrade_follows_a_self_updated_layout_and_keeps_the_old_version() {
    let u = Upgrade::new();
    u.versioned("2.320.0");
    // A completed self-update leaves _work/_update behind; that doesn't block an upgrade.
    std::fs::create_dir_all(u.root.join("_work/_update")).unwrap();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(u.link("bin"), "bin.2.330.0");
    assert_eq!(u.link("externals"), "externals.2.330.0");
    assert_eq!(u.version(), "2.330.0");
    assert!(
        u.root.join("bin.2.320.0/Runner.Listener").exists(),
        "the old version is the rollback copy"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_swap_points_the_links_back_at_the_old_version() {
    let u = Upgrade::new();
    u.versioned("2.320.0");
    // `ln` fails the first time it's asked to repoint externals.
    let flag = u.home.join("ln-failed");
    write_exec(
        &u.shims.join("ln"),
        &format!(
            "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in */externals) if [ ! -e '{}' ]; then touch '{}'; echo simulated >&2; exit 1; fi ;; esac\nexec /bin/ln \"$@\"\n",
            flag.display(),
            flag.display()
        ),
    );
    let (code, _, err) = u.run();
    assert_ne!(code, 0);
    assert!(err.contains("restored the previous bin and externals"), "{err}");
    assert_eq!(
        (u.link("bin"), u.link("externals")),
        ("bin.2.320.0".to_string(), "externals.2.320.0".to_string())
    );
    assert_eq!((u.version(), u.externals()), ("2.320.0".to_string(), "old".to_string()));
    assert!(!u.root.join(".fleet-upgrade-swap").exists());
}

#[cfg(unix)]
#[test]
fn an_interrupted_swap_is_rolled_back_by_the_next_upgrade() {
    // The connection dropped after bin moved to the new version but before externals did.
    let u = Upgrade::new();
    u.versioned("2.320.0");
    write_exec(&u.root.join("bin.2.330.0/Runner.Listener"), &listener("2.330.0"));
    std::fs::remove_file(u.root.join("bin")).unwrap();
    std::os::unix::fs::symlink(u.root.join("bin.2.330.0"), u.root.join("bin")).unwrap();
    std::fs::write(
        u.root.join(".fleet-upgrade-swap"),
        "bin=bin.2.320.0\nexternals=externals.2.320.0\n",
    )
    .unwrap();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("rolled back an interrupted upgrade"), "{out}");
    assert_eq!((u.version(), u.externals()), ("2.330.0".to_string(), "new".to_string()));
    assert!(
        u.root.join("bin.2.320.0/Runner.Listener").exists(),
        "the old version was never removed"
    );

    // A plain folder renamed aside, before its link was made: put back.
    let u = Upgrade::new();
    std::fs::rename(u.root.join("bin"), u.root.join("bin.2.320.0")).unwrap();
    std::fs::write(
        u.root.join(".fleet-upgrade-swap"),
        "bin=bin.2.320.0\nexternals=externals.2.320.0\n",
    )
    .unwrap();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(u.version(), "2.330.0");
    let kept = Command::new(u.root.join("bin.2.320.0/Runner.Listener"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&kept.stdout).trim(), "2.320.0");
}

#[cfg(unix)]
#[test]
fn upgrade_refuses_escaping_links_a_running_service_a_self_update_and_a_cleanup() {
    let u = Upgrade::new();
    let outside = u.home.join("outside-bin");
    std::fs::rename(u.root.join("bin"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, u.root.join("bin")).unwrap();
    let (code, _, err) = u.run();
    assert_eq!(code, 3);
    assert!(err.contains("outside the runner directory"), "{err}");

    // A self-update that started and hasn't finished.
    let u = Upgrade::new();
    std::fs::create_dir_all(u.root.join("_work")).unwrap();
    std::fs::write(u.root.join("_work/_update.sh"), "#!/bin/bash\n").unwrap();
    let (code, _, err) = u.run();
    assert_eq!(code, 5);
    assert!(err.contains("self-update"), "{err}");
    // Finished: it goes ahead.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(u.root.join("update.finished"), "").unwrap();
    assert_eq!(u.run().0, 0);

    let u = Upgrade::new();
    let _service = Running::start(&u.root.join("bin/run-service.sh"));
    let (code, _, err) = u.run();
    assert_eq!(code, 5);
    assert!(err.contains("still running"), "{err}");
    assert_eq!(u.version(), "2.320.0");

    let u = Upgrade::new();
    let holder = Running::start(&u.home.join("elsewhere/holder.sh"));
    std::fs::write(u.root.join(".fleet-maintenance"), holder.0.id().to_string()).unwrap();
    let (code, _, err) = u.run();
    assert_eq!(code, 5);
    assert!(err.contains("a cleanup is still working"), "{err}");
}

/// The start script every change and the watchdog use refuses while a live
/// process holds the maintenance lock, and clears a stale one.
#[cfg(unix)]
#[test]
fn starting_a_runner_waits_for_a_cleanup_holding_its_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let inventory = example_inventory();
    let host = inventory.find_host("mini").unwrap();
    let root = home.join("gh-runners/acme/runner-01");
    write_exec(&root.join("svc.sh"), "#!/bin/sh\ntouch started\n");
    let (commands, client) = recording_remote();
    client.start(host, &host.runners[0], &Transient::NONE).unwrap();
    let script = commands.last().unwrap().input.unwrap();

    let holder = Running::start(&home.join("elsewhere/holder.sh"));
    std::fs::write(root.join(".fleet-maintenance"), holder.0.id().to_string()).unwrap();
    let (code, _, err) = bash(&script, &[("HOME", &home)], None);
    assert_eq!(code, 5, "{err}");
    assert!(err.contains("a cleanup is still working"), "{err}");
    assert!(!root.join("started").exists());
    drop(holder);
    let (code, _, err) = bash(&script, &[("HOME", &home)], None);
    assert_eq!(code, 0, "{err}");
    assert!(root.join("started").exists() && !root.join(".fleet-maintenance").exists());
}

#[test]
fn windows_scripts_never_use_cmd_and_fail_closed_on_unreadable_processes() {
    for script in [CLEANUP_PS1, UPGRADE_PS1] {
        let code: String = script.lines().filter(|l| !l.trim_start().starts_with('#')).collect();
        assert!(
            !code.contains("cmd.exe") && !code.contains("rmdir"),
            "names never pass through cmd.exe"
        );
        assert!(
            code.contains("Get-CimInstance Win32_Process"),
            "processes are attributed through CIM"
        );
        assert!(
            code.contains("-not $where -and $_.Name -in @('Runner.Listener.exe', 'Runner.Worker.exe')"),
            "a runner process whose path can't be read counts as running"
        );
        assert!(code.contains(".fleet-maintenance"), "the maintenance lock is honoured");
        assert!(code.contains("_work\\_update.cmd") && code.contains("update.finished"));
    }
    let inventory = example_inventory();
    let host = inventory.find_host("windows").unwrap();
    let stop = super::super::remote::stop_script(host, &host.runners[0], &Transient::NONE);
    assert!(stop.contains("Get-CimInstance Win32_Process") && stop.contains("-not $where -or"));
    let (commands, client) = recording_remote();
    client.start(host, &host.runners[0], &Transient::NONE).unwrap();
    assert!(
        decoded(&commands).contains(".fleet-maintenance"),
        "Windows starts honour the lock too"
    );
}

/// Runs the PowerShell cleanup against a directory with awkward names and a
/// symlink pointing outside, when `pwsh` is installed (skipped otherwise).
#[cfg(unix)]
#[test]
fn powershell_cleanup_removes_odd_names_and_never_follows_links() {
    let Ok(probe) = Command::new("pwsh").args(["-NoProfile", "-Command", "exit 0"]).output() else {
        eprintln!("pwsh isn't installed; skipping");
        return;
    };
    assert!(probe.status.success());
    let (_tmp, root, _) = runner_dir();
    let outside = root.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), "keep").unwrap();
    for name in ["x&calc", "a^b", "%PATH%", "[brackets]", "it's"] {
        std::fs::create_dir_all(root.join("_work/_temp").join(name).join("inner")).unwrap();
        std::fs::write(root.join("_work/_temp").join(name).join("inner/f"), "x").unwrap();
    }
    std::os::unix::fs::symlink(&outside, root.join("_work/_temp/link")).unwrap();
    let temp = root.join("_work/_temp").display().to_string();
    let script = format!(
        "$env:RF_MODE = 'clean'\n$env:RF_DIAG_DAYS = '14'\n{CLEANUP_PS1}\ntry {{ Invoke-RunnerTarget 'work-temp' 'linux-01' {} {} @() }} finally {{ Unlock-All }}\n",
        super::super::py::ps_quote(&root.display().to_string()),
        super::super::py::ps_quote(&temp)
    );
    let mut child = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
    assert!(out.contains("RFX\twork-temp"), "{out}");
    assert_eq!(std::fs::read_dir(root.join("_work/_temp")).unwrap().count(), 0);
    assert_eq!(std::fs::read_to_string(outside.join("keep")).unwrap(), "keep");
}
