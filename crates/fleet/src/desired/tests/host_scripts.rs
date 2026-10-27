//! The real cleanup and upgrade scripts, run with bash (and pwsh when it's
//! installed) against throwaway runner directories. No SSH and no network:
//! `curl` is replaced on PATH, and nothing outside the temporary directory is
//! touched.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::super::maintenance::{CleanupKind, CleanupOptions, CleanupTarget, cleanup_candidates};
use super::super::model::RunnerRole;
use super::super::remote::{FleetRemote as _, clean_script, parse_clean, parse_survey, survey_script};
use super::*;

const UPGRADE_SH: &str = include_str!("../../../scripts/worker-upgrade.sh");
const CLEANUP_PS1: &str = include_str!("../../../scripts/worker-cleanup.ps1");
const UPGRADE_PS1: &str = include_str!("../../../scripts/worker-upgrade.ps1");

/// The bash the real-script tests run: `FLEET_TEST_BASH` (for example a bash
/// 3.2 build, like macOS's /bin/bash), or `bash` from PATH.
fn bash_program() -> String {
    std::env::var("FLEET_TEST_BASH").unwrap_or_else(|_| "bash".into())
}

/// Runs `script` with bash, extra environment and PATH entries; returns (exit code, stdout, stderr).
fn bash(script: &str, env: &[(&str, &Path)], path_first: Option<&Path>) -> (i32, String, String) {
    let mut command = Command::new(bash_program());
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
        std::fs::write(package.join("run.sh"), "new run.sh").unwrap();
        std::fs::write(package.join("config.sh"), "new config.sh").unwrap();
        std::fs::write(package.join(".env"), "never copied").unwrap();
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
    std::fs::write(u.root.join("run.sh"), "old run.sh").unwrap();
    std::fs::write(u.root.join(".env"), "LANG=C").unwrap();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    // The root files a self-update refreshes; never dotfiles like .env.
    assert_eq!(std::fs::read_to_string(u.root.join("run.sh")).unwrap(), "new run.sh");
    assert_eq!(
        std::fs::read_to_string(u.root.join("config.sh")).unwrap(),
        "new config.sh"
    );
    assert_eq!(std::fs::read_to_string(u.root.join(".env")).unwrap(), "LANG=C");
    assert!(
        !u.root.join(".fleet-maintenance").exists(),
        "the upgrade releases its lock"
    );
    assert_eq!((u.version(), u.externals()), ("2.330.0".to_string(), "new".to_string()));
    assert_eq!(
        u.entries(),
        [
            ".credentials",
            ".env",
            ".runner",
            "bin",
            "bin.2.320.0",
            "bin.2.330.0",
            "config.sh",
            "externals",
            "externals.2.320.0",
            "externals.2.330.0",
            "run.sh",
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
    assert!(err.contains("is still working"), "{err}");
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
    assert!(err.contains("is still working"), "{err}");
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
        assert!(
            code.contains("TrimEnd('\\') + '\\'"),
            "roots match with a trailing separator"
        );
    }
    let inventory = example_inventory();
    let host = inventory.find_host("windows").unwrap();
    let stop = super::super::remote::stop_script(host, &host.runners[0], &Transient::NONE);
    assert!(stop.contains("Get-CimInstance Win32_Process") && stop.contains("-not $where -or"));
    let (commands, client) = recording_remote();
    client.start(host, &host.runners[0], &Transient::NONE).unwrap();
    let start = decoded(&commands);
    assert!(
        start.contains(".fleet-maintenance"),
        "Windows starts honour the lock too"
    );
    assert!(start.contains(".fleet-upgrade-swap"), "and refuse mid-swap");
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

/// A slow `find -delete` stands in for a huge removal: at the script's
/// deadline its whole process group is killed and waited for, the row says it
/// stopped part-way, and the lock is released only then.
#[cfg(unix)]
#[test]
fn the_deadline_stops_the_whole_removal_before_releasing_the_lock() {
    let (_tmp, root, host) = runner_dir();
    let shims = root.parent().unwrap().join("shims");
    let pid_file = root.parent().unwrap().join("find.pid");
    write_exec(
        &shims.join("find"),
        &format!(
            "#!/bin/sh\nfor a; do if [ \"$a\" = -delete ]; then echo $$ > '{}'; exec sleep 60; fi; done\nexec /usr/bin/find \"$@\"\n",
            pid_file.display()
        ),
    );
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(&host, &options)
        .into_iter()
        .filter(|c| c.kind == CleanupKind::WorkTemp)
        .collect();
    let survey = parse_survey(&bash(&survey_script(&host, &candidates, &options), &[], None).1).unwrap();
    let script = clean_script(&host, &survey.targets, &options)
        .unwrap()
        .replacen("RF_SECONDS=840", "RF_SECONDS=2", 1);
    let started = std::time::Instant::now();
    let outcomes = parse_clean(&bash(&script, &[], Some(&shims)).1).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    assert!(outcomes[0].timed_out && !outcomes[0].ok, "{outcomes:?}");
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let alive = Command::new("kill")
        .args(["-0", pid.trim()])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the removal was killed, not orphaned");
    assert!(!root.join(".fleet-maintenance").exists());
}

/// A missing or dangling bin (a self-update that removed the link and failed
/// to make the new one) can't tell the version in use apart: nothing is offered.
#[cfg(unix)]
#[test]
fn previous_versions_need_a_resolvable_bin() {
    let (_tmp, root, host) = runner_dir();
    std::fs::remove_dir_all(root.join("bin")).unwrap();
    for version in ["2.320.0", "2.330.0"] {
        std::fs::create_dir_all(root.join(format!("bin.{version}/x"))).unwrap();
    }
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(&host, &options)
        .into_iter()
        .filter(|c| c.kind == CleanupKind::PreviousBin)
        .collect();
    for dangling in [false, true] {
        if dangling {
            std::os::unix::fs::symlink(root.join("bin.2.999.0"), root.join("bin")).unwrap();
        }
        let survey = parse_survey(&bash(&survey_script(&host, &candidates, &options), &[], None).1).unwrap();
        assert!(survey.targets.is_empty(), "dangling={dangling}: {survey:?}");
        assert!(
            survey
                .refused
                .iter()
                .any(|(_, _, m)| m.contains("previous versions are kept"))
        );
        let reviewed = CleanupTarget {
            kind: CleanupKind::PreviousBin,
            runner: Some("linux-01".into()),
            path: Some(root.display().to_string()),
            kib: None,
            count: None,
            detail: None,
            files: vec!["bin.2.320.0".into(), "bin.2.330.0".into()],
        };
        let outcomes = parse_clean(&bash(&clean_script(&host, &[reviewed], &options).unwrap(), &[], None).1).unwrap();
        assert!(!outcomes[0].ok, "{outcomes:?}");
        assert!(root.join("bin.2.320.0/x").exists() && root.join("bin.2.330.0/x").exists());
    }
}

#[cfg(unix)]
#[test]
fn leftover_upgrade_staging_folders_are_offered_unless_an_upgrade_is_live() {
    let (_tmp, root, host) = runner_dir();
    std::fs::create_dir_all(root.join(".fleet-upgrade.Ab12Cd/new")).unwrap();
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(&host, &options)
        .into_iter()
        .filter(|c| c.kind == CleanupKind::UpgradeLeftovers)
        .collect();
    let survey = || parse_survey(&bash(&survey_script(&host, &candidates, &options), &[], None).1).unwrap();
    // While an upgrade holds the lock, its staging folder isn't offered.
    {
        let holder = Running::start(&root.parent().unwrap().join("elsewhere/holder.sh"));
        std::fs::write(root.join(".fleet-maintenance"), holder.0.id().to_string()).unwrap();
        assert!(survey().targets.is_empty());
    }
    let found = survey();
    assert_eq!(found.targets[0].files, [".fleet-upgrade.Ab12Cd"]);
    let mut reviewed = found.targets.clone();
    reviewed[0].files.push("../escape".into());
    let outcomes = parse_clean(&bash(&clean_script(&host, &reviewed, &options).unwrap(), &[], None).1).unwrap();
    assert!(outcomes.iter().any(|o| o.ok), "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| !o.ok && o.message.contains("isn't an upgrade's staging folder"))
    );
    assert!(!root.join(".fleet-upgrade.Ab12Cd").exists());
}

#[cfg(unix)]
#[test]
fn a_rollback_that_cant_relink_keeps_its_marker_and_fails() {
    let u = Upgrade::new();
    u.versioned("2.320.0");
    std::fs::create_dir_all(u.root.join("bin.2.330.0")).unwrap();
    std::fs::remove_file(u.root.join("bin")).unwrap();
    std::os::unix::fs::symlink(u.root.join("bin.2.330.0"), u.root.join("bin")).unwrap();
    std::fs::write(
        u.root.join(".fleet-upgrade-swap"),
        "bin=bin.2.320.0\nexternals=externals.2.320.0\n",
    )
    .unwrap();
    write_exec(&u.shims.join("ln"), "#!/bin/sh\necho simulated >&2\nexit 1\n");
    let (code, out, err) = u.run();
    assert_eq!(code, 3, "{out}{err}");
    assert!(err.contains("couldn't be rolled back"), "{err}");
    assert!(!out.contains("rolled back"), "{out}");
    assert!(
        u.root.join(".fleet-upgrade-swap").exists(),
        "the marker stays until every link is back"
    );
    assert!(!u.root.join(".fleet-maintenance").exists());
}

/// Something started from the runner directory while the release downloaded:
/// the re-check right before the swap refuses, and nothing is switched.
#[cfg(unix)]
#[test]
fn upgrade_rechecks_processes_right_before_the_swap_and_holds_its_lock() {
    let u = Upgrade::new();
    let archive = u.home.join("runner.tar.gz");
    let late = u.root.join("bin/late-start.sh");
    write_exec(&late, "#!/bin/sh\nsleep 30\n");
    let seen = u.home.join("lock-seen");
    let pid_file = u.home.join("late.pid");
    write_exec(
        &u.shims.join("curl"),
        &format!(
            "#!/bin/sh\n[ -f '{lock}' ] && touch '{seen}'\nsh '{late}' > /dev/null 2>&1 &\necho $! > '{pid}'\n\
while [ $# -gt 0 ]; do [ \"$1\" = --output ] && cp '{archive}' \"$2\"; shift; done\n",
            lock = u.root.join(".fleet-maintenance").display(),
            seen = seen.display(),
            late = late.display(),
            pid = pid_file.display(),
            archive = archive.display(),
        ),
    );
    let (code, _, err) = u.run();
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    let _ = Command::new("kill").arg(pid.trim()).status();
    assert!(seen.exists(), "the upgrade held the maintenance lock while it worked");
    assert_eq!(code, 5, "{err}");
    assert!(err.contains("still running"), "{err}");
    assert!(!u.root.join(".fleet-upgrade-swap").exists());
    assert!(
        u.root.join("bin").is_dir() && !u.root.join("bin").is_symlink(),
        "nothing was switched"
    );
    assert!(!u.root.join(".fleet-maintenance").exists());
}

#[cfg(unix)]
#[test]
fn starting_a_runner_refuses_while_an_upgrade_is_mid_swap() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().canonicalize().unwrap();
    let inventory = example_inventory();
    let host = inventory.find_host("mini").unwrap();
    let root = home.join("gh-runners/acme/runner-01");
    write_exec(&root.join("svc.sh"), "#!/bin/sh\ntouch started\n");
    std::fs::write(root.join(".fleet-upgrade-swap"), "bin=bin.2.320.0\n").unwrap();
    let (commands, client) = recording_remote();
    client
        .restart(host, &host.runners[0], RunnerRole::Declared, &Transient::NONE)
        .unwrap();
    let script = commands.last().unwrap().input.unwrap();
    let (code, _, err) = bash(&script, &[("HOME", &home)], None);
    assert_eq!(code, 5, "{err}");
    assert!(err.contains("interrupted mid-swap"), "{err}");
    assert!(!root.join("started").exists());
}

/// A clean script whose `find -delete` is replaced by `body`, with a short deadline.
fn shimmed_clean(root: &Path, host: &Host, kinds: &[CleanupKind], body: &str, seconds: u32) -> (String, PathBuf) {
    let shims = root.parent().unwrap().join("shims");
    write_exec(
        &shims.join("find"),
        &format!("#!/bin/sh\nfor a; do if [ \"$a\" = -delete ]; then {body}; fi; done\nexec /usr/bin/find \"$@\"\n"),
    );
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(host, &options)
        .into_iter()
        .filter(|c| kinds.contains(&c.kind))
        .collect();
    let survey = parse_survey(&bash(&survey_script(host, &candidates, &options), &[], None).1).unwrap();
    let script = clean_script(host, &survey.targets, &options).unwrap().replacen(
        "RF_SECONDS=840",
        &format!("RF_SECONDS={seconds}"),
        1,
    );
    (script, shims)
}

/// A removal that reads stdin must not swallow the rest of the script, which
/// arrives on the same SSH pipe: the following target still runs.
#[cfg(unix)]
#[test]
fn a_removal_reading_stdin_doesnt_swallow_the_rest_of_the_script() {
    let (_tmp, root, host) = runner_dir();
    std::fs::create_dir_all(root.join("_work/_actions/a")).unwrap();
    let (script, shims) = shimmed_clean(
        &root,
        &host,
        &[CleanupKind::WorkTemp, CleanupKind::WorkActions],
        "cat > /dev/null; exit 0",
        60,
    );
    let outcomes = parse_clean(&bash(&script, &[], Some(&shims)).1).unwrap();
    let kinds: Vec<CleanupKind> = outcomes.iter().filter(|o| o.ok).map(|o| o.kind).collect();
    assert_eq!(kinds, [CleanupKind::WorkTemp, CleanupKind::WorkActions], "{outcomes:?}");
}

/// TERM to the script mid-removal stops the removal's whole process group and
/// waits for it before the lock is released.
#[cfg(unix)]
#[test]
fn terminating_the_script_stops_the_removal_before_releasing_the_lock() {
    let (_tmp, root, host) = runner_dir();
    let pid_file = root.parent().unwrap().join("find.pid");
    let (script, shims) = shimmed_clean(
        &root,
        &host,
        &[CleanupKind::WorkTemp],
        &format!("echo $$ > '{}'; exec sleep 60", pid_file.display()),
        600,
    );
    let path = format!("{}:{}", shims.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new(bash_program())
        .arg("-s")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    let lock = root.join(".fleet-maintenance");
    for _ in 0..100 {
        if pid_file.exists() && lock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let find_pid = std::fs::read_to_string(&pid_file).unwrap();
    let alive = |pid: &str| {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success()
    };
    assert!(alive(&find_pid));
    Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    // While the lock exists the removal may still run; once it's gone, the removal must be too.
    for _ in 0..200 {
        if !lock.exists() {
            assert!(!alive(&find_pid), "the lock was released while the removal still ran");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(!lock.exists());
    assert!(!alive(&find_pid));
}

/// Sizing used up the time: the removal isn't started (a clean refusal, not
/// a removal stopped part-way), and nothing is removed.
#[cfg(unix)]
#[test]
fn a_removal_isnt_started_after_the_deadline() {
    let (_tmp, root, host) = runner_dir();
    let (script, shims) = shimmed_clean(&root, &host, &[CleanupKind::WorkTemp], "exit 0", 2);
    write_exec(&shims.join("du"), "#!/bin/sh\nexec sleep 60\n");
    let outcomes = parse_clean(&bash(&script, &[], Some(&shims)).1).unwrap();
    assert!(!outcomes[0].ok && !outcomes[0].timed_out, "{outcomes:?}");
    assert!(outcomes[0].message.contains("ran out of time before"), "{outcomes:?}");
    assert!(root.join("_work/_temp/job/file").exists());
}
