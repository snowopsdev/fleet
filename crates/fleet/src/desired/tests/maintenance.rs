//! Cleanup, upgrade, drain and resume against fake GitHub and hosts, the
//! scripts they send, and the cleanup script run on a throwaway directory.

use std::collections::BTreeSet;

use super::super::github::ReleaseAsset;
use super::super::maintenance::{
    CleanupKind, CleanupOptions, CleanupTarget, StopOutcome, cleanup_candidates, version_behind,
};
use super::super::remote::{clean_script, parse_clean, parse_survey, stop_script, survey_script};
use super::*;

fn target(kind: CleanupKind, runner: Option<&str>, path: &str) -> CleanupTarget {
    CleanupTarget {
        kind,
        runner: runner.map(String::from),
        path: (!path.is_empty()).then(|| path.to_string()),
        kib: Some(2048),
        count: None,
        detail: None,
        files: Vec::new(),
    }
}

/// The example `linux` host's runner, online and idle, with an active service.
fn linux_fleet(inventory: &Inventory, version: &str) -> FakeFleet {
    let host = inventory.find_host("linux").unwrap();
    let runner = &host.runners[0];
    FakeFleet::new(
        vec![gr(&runner.name, "online", false, &[], Some(true))],
        vec![LocalRunner::new(
            &runner.name,
            &host.runner_path(runner),
            true,
            "active",
            Some(version),
        )],
    )
}

fn asset(version: &str) -> ReleaseAsset {
    ReleaseAsset {
        version: version.into(),
        name: "actions-runner-linux-x64.tar.gz".into(),
        url: "https://github.com/actions/runner/releases/download/v2.999.0/actions-runner-linux-x64.tar.gz".into(),
        sha256: Some("a".repeat(64)),
    }
}

#[test]
fn versions_compare_numerically_and_unknown_is_never_behind() {
    assert!(version_behind("2.329.0", "2.330.0"));
    assert!(version_behind("2.99.0", "2.100.0"));
    assert!(!version_behind("2.330.0", "2.330.0"));
    assert!(!version_behind("2.331.0", "2.330.0"));
    assert!(!version_behind("unknown", "2.330.0"));
}

#[test]
fn candidates_are_fixed_per_platform() {
    let inventory = example_inventory();
    let kinds = |host: &str, whole_work: bool| -> Vec<&'static str> {
        let options = CleanupOptions {
            whole_work,
            ..CleanupOptions::default()
        };
        let mut kinds: Vec<&str> = cleanup_candidates(inventory.find_host(host).unwrap(), &options)
            .iter()
            .map(|c| c.kind.as_str())
            .collect();
        kinds.dedup();
        kinds
    };
    assert_eq!(
        kinds("mini", false),
        [
            "work-temp",
            "work-actions",
            "diag",
            "previous-bin",
            "previous-externals",
            "xcode-derived-data",
            "simulators",
            "docker"
        ]
    );
    assert_eq!(
        kinds("linux", true),
        ["work", "diag", "previous-bin", "previous-externals", "docker"]
    );
    assert_eq!(
        kinds("windows", false),
        [
            "work-temp",
            "work-actions",
            "diag",
            "previous-bin",
            "previous-externals"
        ]
    );
}

#[test]
fn cleanup_skips_a_runner_that_became_busy_and_runs_the_rest() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.330.0");
    let targets = vec![
        target(
            CleanupKind::WorkTemp,
            Some("linux-01"),
            "/home/builder/gh-runners/acme/runner-01/_work/_temp",
        ),
        target(CleanupKind::Docker, None, ""),
    ];
    fleet.world().github.get_mut("linux-01").unwrap().busy = true;
    let error = ops(&inventory, &fleet, Secrets::default())
        .cleanup_host("linux", &CleanupOptions::default(), &targets, &BTreeSet::new(), &mut Vec::new())
        .unwrap_err();
    assert!(error.0.contains("linux-01 is busy"), "{error}");
    assert!(fleet.world().calls.is_empty(), "nothing is removed while a job runs");

    fleet.world().github.get_mut("linux-01").unwrap().busy = false;
    let messages = ops(&inventory, &fleet, Secrets::default())
        .cleanup_host("linux", &CleanupOptions::default(), &targets, &BTreeSet::new(), &mut Vec::new())
        .unwrap();
    assert_eq!(messages.len(), 2);
    // The runner is stopped around the removal, so it can't take a job meanwhile, then started again.
    assert_eq!(
        fleet.world().calls,
        [
            "stop linux linux-01",
            "cleanup linux work-temp /home/builder/gh-runners/acme/runner-01/_work/_temp",
            "cleanup linux docker -",
            "start linux linux-01"
        ]
    );
}

#[test]
fn cleanup_restores_each_runner_as_it_was_and_leaves_drained_ones_alone() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.330.0");
    let temp = target(CleanupKind::WorkTemp, Some("linux-01"), "/r/_work/_temp");
    let docker = target(CleanupKind::Docker, None, "");
    let held: BTreeSet<String> = ["linux-01".to_string()].into();
    let options = CleanupOptions::default();
    // Draining and still running: neither its directories nor shared caches are touched.
    let error = ops(&inventory, &fleet, Secrets::default())
        .cleanup_host("linux", &options, &[temp.clone(), docker], &held, &mut Vec::new())
        .unwrap_err();
    assert!(error.0.contains("is draining") && error.0.contains("machine-wide"), "{error}");
    assert!(fleet.world().calls.is_empty());

    // Stopped by its drain: cleaned without being started.
    fleet.world().local.get_mut("linux-01").unwrap().service_state = "inactive".into();
    ops(&inventory, &fleet, Secrets::default())
        .cleanup_host("linux", &options, std::slice::from_ref(&temp), &held, &mut Vec::new())
        .unwrap();
    assert_eq!(fleet.world().calls, ["cleanup linux work-temp /r/_work/_temp"]);

    // Running, not drained, and it fails to start again: reported and left held.
    fleet.world().calls.clear();
    fleet.world().local.get_mut("linux-01").unwrap().service_state = "active".into();
    fleet.world().start_failures.push("linux-01".into());
    let mut left = Vec::new();
    let error = ops(&inventory, &fleet, Secrets::default())
        .cleanup_host("linux", &options, &[temp], &BTreeSet::new(), &mut left)
        .unwrap_err();
    assert!(error.0.contains("couldn't be started again"), "{error}");
    assert_eq!(left, ["linux-01"]);
}

#[test]
fn cleanup_reports_targets_the_host_refused() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.330.0");
    fleet.world().jobs_on_host.push("linux-01".into());
    let targets = [target(CleanupKind::WorkTemp, Some("linux-01"), "/x/_work/_temp")];
    let error = ops(&inventory, &fleet, Secrets::default())
        .cleanup_host("linux", &CleanupOptions::default(), &targets, &BTreeSet::new(), &mut Vec::new())
        .unwrap_err();
    assert!(error.0.contains("a job started"), "{error}");
    assert!(fleet.world().calls.is_empty(), "a runner with a job is neither stopped nor cleaned");
}

#[test]
fn drain_stops_an_idle_runner_and_leaves_a_busy_one_running() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.330.0");
    fleet.world().github.get_mut("linux-01").unwrap().busy = true;
    let outcome = ops(&inventory, &fleet, Secrets::default())
        .stop_if_idle("linux-01", None)
        .unwrap();
    assert_eq!(outcome, StopOutcome::Busy { registration: 11 });
    assert!(fleet.world().calls.is_empty());

    // Idle on GitHub, but a job just started on the host: still busy, nothing stopped.
    fleet.world().github.get_mut("linux-01").unwrap().busy = false;
    fleet.world().jobs_on_host.push("linux-01".into());
    let outcome = ops(&inventory, &fleet, Secrets::default())
        .stop_if_idle("linux-01", None)
        .unwrap();
    assert_eq!(outcome, StopOutcome::Busy { registration: 11 });

    fleet.world().jobs_on_host.clear();
    let outcome = ops(&inventory, &fleet, Secrets::default())
        .stop_if_idle("linux-01", Some(11))
        .unwrap();
    assert!(matches!(outcome, StopOutcome::Stopped { registration: 11, .. }));
    assert_eq!(fleet.world().calls, ["stop linux linux-01"]);

    // A deferred stop refuses a runner registered again since the review.
    let error = ops(&inventory, &fleet, Secrets::default())
        .stop_if_idle("linux-01", Some(99))
        .unwrap_err();
    assert!(error.0.contains("registered again"), "{error}");
}

#[test]
fn drain_and_resume_refuse_legacy_and_decommissioned_runners() {
    let inventory = example_inventory();
    let fleet = FakeFleet::default();
    let error = ops(&inventory, &fleet, Secrets::default())
        .stop_if_idle("Legacy-Studio", None)
        .unwrap_err();
    assert!(error.0.contains("legacy runner"), "{error}");
    let error = ops(&inventory, &fleet, Secrets::default())
        .resume_runner("Legacy-Studio")
        .unwrap_err();
    assert!(error.0.contains("legacy runner"), "{error}");
    assert!(fleet.world().calls.is_empty());
}

#[test]
fn upgrade_stops_replaces_starts_and_confirms_the_version() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.329.0");
    let messages = ops(&inventory, &fleet, Secrets::default())
        .upgrade_runners("linux", &["linux-01".into()], &asset("2.999.0"), &mut Vec::new())
        .unwrap();
    assert_eq!(messages, ["linux-01 upgraded to 2.999.0 and is online"]);
    assert_eq!(
        fleet.world().calls,
        [
            "stop linux linux-01",
            "upgrade linux linux-01 2.999.0",
            "start linux linux-01"
        ]
    );
}

#[test]
fn a_failed_upgrade_leaves_the_runner_stopped_with_recovery_steps() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.329.0");
    fleet.world().upgrade_failures.push("linux-01".into());
    let mut left = Vec::new();
    let error = ops(&inventory, &fleet, Secrets::default())
        .upgrade_runners("linux", &["linux-01".into()], &asset("2.999.0"), &mut left)
        .unwrap_err();
    assert_eq!(left, ["linux-01"], "the failure is reported so the watchdog can leave it alone");
    assert!(
        error.0.starts_with("upgrade failed for linux-01: SHA-256 mismatch"),
        "{error}"
    );
    assert!(
        error.0.contains("left stopped") && error.0.contains("bin.fleet-previous"),
        "{error}"
    );
    assert_eq!(fleet.world().local["linux-01"].service_state, "inactive");
    assert!(!fleet.world().calls.iter().any(|c| c.starts_with("start")));
}

#[test]
fn upgrade_never_touches_a_busy_runner_and_needs_a_digest() {
    let inventory = example_inventory();
    let fleet = linux_fleet(&inventory, "2.329.0");
    fleet.world().github.get_mut("linux-01").unwrap().busy = true;
    let messages = ops(&inventory, &fleet, Secrets::default())
        .upgrade_runners("linux", &["linux-01".into()], &asset("2.999.0"), &mut Vec::new())
        .unwrap();
    assert_eq!(messages, ["skipped linux-01: busy"]);
    assert!(fleet.world().calls.is_empty());
    let mut unverified = asset("2.999.0");
    unverified.sha256 = None;
    let error = ops(&inventory, &fleet, Secrets::default())
        .upgrade_runners("linux", &["linux-01".into()], &unverified, &mut Vec::new())
        .unwrap_err();
    assert!(error.0.contains("SHA-256"), "{error}");
}

#[test]
fn cleanup_scripts_name_exact_targets_without_globs() {
    let inventory = example_inventory();
    let host = inventory.find_host("linux").unwrap();
    let options = CleanupOptions::default();
    let survey = survey_script(host, &cleanup_candidates(host, &options), &options);
    assert!(survey.starts_with("RF_MODE=survey\nRF_DIAG_DAYS=14\n"));
    assert!(survey.contains("runner_target work-temp linux-01 \"$HOME\"/gh-runners/acme/runner-01 ''\n"));
    assert!(survey.contains("machine_target docker ''\n"));
    let mut diag = target(
        CleanupKind::Diag,
        Some("linux-01"),
        "/home/b/gh-runners/acme/runner-01/_diag",
    );
    diag.files = vec!["Runner_20260101-000000-utc.log".into()];
    let clean = clean_script(host, &[diag], &options).unwrap();
    assert!(clean.contains(
        "runner_target diag linux-01 \"$HOME\"/gh-runners/acme/runner-01 /home/b/gh-runners/acme/runner-01/_diag Runner_20260101-000000-utc.log\n"
    ));
    assert!(!clean.contains("rm -rf"));
    // A target for a runner the host doesn't declare is refused before anything is sent.
    let stray = target(CleanupKind::WorkTemp, Some("studio-01"), "/x");
    assert!(clean_script(host, &[stray], &options).is_err());

    let windows = inventory.find_host("windows").unwrap();
    let script = survey_script(windows, &cleanup_candidates(windows, &options), &options);
    assert!(script.contains("Invoke-RunnerTarget 'work-temp' 'windows-01' 'C:\\gh-runners\\acme\\runner-01' '' @()"));
    assert!(!script.contains("machine_target"));
}

#[test]
fn stop_scripts_refuse_while_a_job_runs() {
    let inventory = example_inventory();
    let host = inventory.find_host("linux").unwrap();
    let script = stop_script(host, &host.runners[0], &secret("sudo-pass"));
    assert!(script.contains("grep -F -- \"$real/bin/Runner.Worker\""));
    assert!(script.contains("run_as_root ./svc.sh stop"));
    assert!(script.contains("RF_SUDO_PASSWORD=sudo-pass\n"));
    let gui = inventory.find_host("windows-gui").unwrap();
    let script = stop_script(gui, &gui.runners[0], &Transient::NONE);
    assert!(
        script.contains("Disable-ScheduledTask"),
        "a stopped logon task stays disabled"
    );
    assert!(script.contains("a job is running"));
}

#[test]
fn survey_and_clean_output_is_parsed_strictly() {
    let survey = parse_survey(
        "noise\nRFF\tlinux-01\tRunner_1.log\nRFC\tdiag\tlinux-01\t/r/_diag\t12\t1\t\n\
         RFC\tdocker\t\t\t\t\tImages: 1.2GB (40%)\nRFE\twork-temp\tlinux-01\tsymlink; left it alone\n",
    )
    .unwrap();
    assert_eq!(survey.targets.len(), 2);
    assert_eq!(survey.targets[0].files, ["Runner_1.log"]);
    assert_eq!(survey.targets[0].kib, Some(12));
    assert_eq!(survey.targets[1].detail.as_deref(), Some("Images: 1.2GB (40%)"));
    assert_eq!(survey.refused.len(), 1);
    assert!(parse_survey("RFC\tnot-a-kind\t\t\t\t\t\n").is_err());
    assert!(parse_survey("RFC\twork\tonly-three\n").is_err());

    let outcomes =
        parse_clean("RFX\twork-temp\tlinux-01\t/r/_work/_temp\t2097152\t\t\nRFE\tdocker\t\tDocker isn't available\n")
            .unwrap();
    assert_eq!(outcomes[0].message, "emptied /r/_work/_temp (2.0 GiB)");
    assert!(outcomes[0].ok);
    assert!(!outcomes[1].ok);
}

/// Runs the real cleanup script with bash against a throwaway runner
/// directory. No SSH; nothing outside the temporary directory is touched.
#[cfg(unix)]
#[test]
fn cleanup_script_removes_only_contents_of_exact_targets() {
    use std::io::Write as _;
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("gh-runners/acme/runner-01");
    for dir in [
        "_work/_temp/job",
        "_work/_actions/actions/checkout",
        "_diag",
        "bin",
        "externals",
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    std::fs::create_dir_all(base.join("outside")).unwrap();
    std::fs::write(base.join("outside/keep"), "keep").unwrap();
    std::fs::write(root.join(".runner"), r#"{"agentName": "linux-01"}"#).unwrap();
    std::fs::write(root.join(".credentials"), "secret").unwrap();
    std::fs::write(root.join("_work/_temp/job/file"), "x").unwrap();
    std::fs::write(root.join("_work/_actions/actions/checkout/a"), "x").unwrap();
    symlink(base.join("outside"), root.join("_work/_temp/link")).unwrap();
    symlink(base.join("outside"), root.join("_work/_actions/link")).unwrap();

    let mut inventory = example_inventory();
    let host = inventory.hosts.iter_mut().find(|h| h.id == "linux").unwrap();
    host.root = base.join("gh-runners/acme").display().to_string();
    let host = host.clone();
    let run = |script: String| {
        let mut child = std::process::Command::new("bash")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    };
    let options = CleanupOptions::default();
    let candidates: Vec<_> = cleanup_candidates(&host, &options)
        .into_iter()
        .filter(|c| c.kind.is_runner_target())
        .collect();
    let survey = parse_survey(&run(survey_script(&host, &candidates, &options))).unwrap();
    let kinds: Vec<CleanupKind> = survey.targets.iter().map(|t| t.kind).collect();
    assert_eq!(kinds, [CleanupKind::WorkTemp, CleanupKind::WorkActions]);

    // `_work/_actions` is swapped for a symlink after the review: it is refused.
    std::fs::rename(root.join("_work/_actions"), root.join("_work/actions-real")).unwrap();
    symlink(base.join("outside"), root.join("_work/_actions")).unwrap();
    let outcomes = parse_clean(&run(clean_script(&host, &survey.targets, &options).unwrap())).unwrap();
    assert!(outcomes[0].ok, "{outcomes:?}");
    assert!(
        !outcomes[1].ok && outcomes[1].message.contains("symlink"),
        "{outcomes:?}"
    );

    assert!(root.join("_work/_temp").is_dir(), "the directory itself is kept");
    assert_eq!(std::fs::read_dir(root.join("_work/_temp")).unwrap().count(), 0);
    assert_eq!(std::fs::read_to_string(base.join("outside/keep")).unwrap(), "keep");
    assert!(root.join(".runner").exists() && root.join(".credentials").exists());
    assert!(root.join("bin").is_dir() && root.join("externals").is_dir());
    assert!(root.join("_work/actions-real/actions/checkout/a").exists());
}
