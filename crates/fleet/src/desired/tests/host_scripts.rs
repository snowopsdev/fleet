//! The real cleanup and upgrade scripts, run with bash (and pwsh when it's
//! installed) against throwaway runner directories. No SSH and no network:
//! `curl` is replaced on PATH, and nothing outside the temporary directory is
//! touched.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::super::maintenance::{CleanupKind, CleanupOptions, cleanup_candidates};
use super::super::remote::{clean_script, parse_clean, parse_survey, survey_script};
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
        let _service = Running::start(&root.join("bin/Runner.Listener"));
        let outcomes = clean_temp(&host);
        assert!(
            !outcomes[0].ok && outcomes[0].message.contains("still running"),
            "{outcomes:?}"
        );
    }
    std::fs::create_dir_all(root.join("_work/_update")).unwrap();
    let outcomes = clean_temp(&host);
    assert!(
        !outcomes[0].ok && outcomes[0].message.contains("self-update"),
        "{outcomes:?}"
    );
    assert!(root.join("_work/_temp/job/file").exists(), "nothing was removed");
    std::fs::remove_dir(root.join("_work/_update")).unwrap();
    assert!(clean_temp(&host)[0].ok);
    assert!(!root.join("_work/_temp/job").exists());
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
fn upgrade_replaces_binaries_in_place_and_keeps_the_previous_ones() {
    let u = Upgrade::new();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!((u.version(), u.externals()), ("2.330.0".to_string(), "new".to_string()));
    assert_eq!(
        u.entries(),
        [
            ".credentials",
            ".runner",
            "bin",
            "bin.fleet-previous",
            "externals",
            "externals.fleet-previous"
        ]
    );
    assert_eq!(std::fs::read_to_string(u.root.join(".credentials")).unwrap(), "secret");
    // Already current: nothing changes.
    let (code, out, _) = u.run();
    assert_eq!(code, 0);
    assert!(out.contains("already runs 2.330.0"), "{out}");
}

#[cfg(unix)]
#[test]
fn a_failed_swap_puts_the_previous_files_back() {
    let u = Upgrade::new();
    write_exec(
        &u.shims.join("mv"),
        "#!/bin/sh\ncase \"$1\" in */new/externals) echo 'simulated failure' >&2; exit 1 ;; esac\nexec /bin/mv \"$@\"\n",
    );
    let (code, _, err) = u.run();
    assert_ne!(code, 0);
    assert!(err.contains("restored the previous bin/ and externals/"), "{err}");
    assert_eq!((u.version(), u.externals()), ("2.320.0".to_string(), "old".to_string()));
    assert_eq!(u.entries(), [".credentials", ".runner", "bin", "externals"]);
}

#[cfg(unix)]
#[test]
fn an_interrupted_swap_is_repaired_or_refused_and_previous_files_are_never_lost() {
    // Interrupted after bin/ moved aside: bin is put back, then the upgrade runs.
    let u = Upgrade::new();
    std::fs::rename(u.root.join("bin"), u.root.join("bin.fleet-previous")).unwrap();
    let (code, out, err) = u.run();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("restored"), "{out}");
    assert_eq!(u.version(), "2.330.0");
    let kept = Command::new(u.root.join("bin.fleet-previous/Runner.Listener"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&kept.stdout).trim(), "2.320.0");

    // Interrupted after externals/ moved aside: ambiguous, so it refuses and removes nothing.
    let u = Upgrade::new();
    std::fs::rename(u.root.join("externals"), u.root.join("externals.fleet-previous")).unwrap();
    let (code, _, err) = u.run();
    assert_eq!(code, 3);
    assert!(err.contains("was interrupted"), "{err}");
    assert!(u.root.join("externals.fleet-previous/node/version").exists());
}

#[cfg(unix)]
#[test]
fn upgrade_refuses_symlinks_a_running_service_and_a_self_update() {
    let u = Upgrade::new();
    std::fs::rename(u.root.join("bin"), u.root.join("bin.real")).unwrap();
    std::os::unix::fs::symlink(u.root.join("bin.real"), u.root.join("bin")).unwrap();
    let (code, _, err) = u.run();
    assert_eq!(code, 3);
    assert!(err.contains("symlink"), "{err}");

    let u = Upgrade::new();
    std::fs::create_dir_all(u.root.join("_work/_update")).unwrap();
    let (code, _, err) = u.run();
    assert_eq!(code, 5);
    assert!(err.contains("self-update"), "{err}");

    let u = Upgrade::new();
    let _service = Running::start(&u.root.join("bin/run-service.sh"));
    let (code, _, err) = u.run();
    assert_eq!(code, 5);
    assert!(err.contains("still running"), "{err}");
    assert_eq!(u.version(), "2.320.0");
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
            code.contains("-not $_.Path -or"),
            "a process with an unreadable path counts as running"
        );
        assert!(
            !code.contains("$_.Path -and"),
            "no check skips unreadable process paths"
        );
    }
    let inventory = example_inventory();
    let host = inventory.find_host("windows").unwrap();
    let stop = super::super::remote::stop_script(host, &host.runners[0], &Transient::NONE);
    assert!(stop.contains("-not $_.Path -or"));
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
        "$env:RF_MODE = 'clean'\n$env:RF_DIAG_DAYS = '14'\n{CLEANUP_PS1}\nInvoke-RunnerTarget 'work-temp' 'linux-01' {} {} @()\n",
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
