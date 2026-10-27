use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::publish::{self, ActionsVariables, GhVariables};
use super::store::{DrainMark, DrainState, Journal};
use super::watchdog::{Decision, plan};
use crate::desired::process::{CommandError, CommandResult, CommandRunner, NonZeroExit};
use crate::model::{DataSource, FleetView, MachineConnection, RunnerView};
use crate::sync::MutexExt;

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

/// The demo fleet as live data at `at`, with `linux-01-a`'s service stopped
/// and GitHub showing it offline and idle, every reading fresh.
fn view(at: DateTime<Utc>) -> FleetView {
    let (config, obs) = crate::demo::dataset(at);
    let mut view = crate::derive::derive(
        &config,
        &obs,
        &crate::demo_desired::plan_inputs(at),
        at,
        DataSource::Live,
    );
    let machine = view
        .machines
        .iter_mut()
        .find(|m| m.desired.as_ref().is_some_and(|d| d.host.id == "build-linux-01"))
        .unwrap();
    machine.connection = MachineConnection::Connected { checked_at: at };
    let runner = machine.runners.iter_mut().find(|r| r.name == "linux-01-a").unwrap();
    set_service(runner, "failed");
    let github = runner.github.as_mut().unwrap();
    github.status = "offline".into();
    github.busy = false;
    github.stale = false;
    view
}

fn set_service(runner: &mut RunnerView, state: &str) {
    let local = runner.local.as_mut().unwrap();
    local.service_state = state.into();
    local.configured = true;
    local.stale = false;
}

fn linux(view: &mut FleetView) -> &mut RunnerView {
    view.machines
        .iter_mut()
        .flat_map(|m| m.runners.iter_mut())
        .find(|r| r.name == "linux-01-a")
        .unwrap()
}

/// Watchdog decisions for `linux-01-a` at `minutes` after the start.
fn round(journal: &mut Journal, minutes: i64) -> Vec<String> {
    let at = start() + Duration::minutes(minutes);
    plan(&view(at), journal, at)
        .into_iter()
        .map(|d| match d {
            Decision::Restart { subject, attempt } => format!("restart {} #{attempt}", subject.runner),
            Decision::GiveUp { subject } => format!("give up {}", subject.runner),
            Decision::Recovered { subject, gave_up } => format!("recovered {} gave_up={gave_up}", subject.runner),
        })
        .collect()
}

#[test]
fn watchdog_backs_off_then_gives_up_after_three_attempts() {
    let mut journal = Journal::default();
    // First seen stopped: wait two minutes.
    assert!(round(&mut journal, 0).is_empty());
    assert!(round(&mut journal, 1).is_empty());
    assert_eq!(round(&mut journal, 2), ["restart linux-01-a #1"]);
    // Then 5, 15 and 60 minutes after the previous attempt.
    assert!(round(&mut journal, 6).is_empty());
    assert_eq!(round(&mut journal, 7), ["restart linux-01-a #2"]);
    assert!(round(&mut journal, 21).is_empty());
    assert_eq!(round(&mut journal, 22), ["restart linux-01-a #3"]);
    assert!(round(&mut journal, 81).is_empty());
    assert_eq!(round(&mut journal, 82), ["give up linux-01-a"]);
    // It stays given up: no further attempts, however long it stays down.
    assert!(round(&mut journal, 200).is_empty());
    let state = &journal.watchdog["linux-01-a"];
    assert_eq!(state.attempts.len(), 3);
    assert_eq!(state.gave_up_at, Some(start() + Duration::minutes(82)));
    assert!(round(&mut journal, 500).is_empty(), "even once its attempts age out");
    assert!(journal.watchdog["linux-01-a"].gave_up_at.is_some());
}

#[test]
fn a_recovered_runner_is_reported_and_its_attempts_still_count_for_six_hours() {
    let mut journal = Journal::default();
    round(&mut journal, 0);
    assert_eq!(round(&mut journal, 2), ["restart linux-01-a #1"]);
    let at = start() + Duration::minutes(3);
    let mut running = view(at);
    set_service(linux(&mut running), "active");
    let decisions = plan(&running, &mut journal, at);
    assert!(matches!(&decisions[..], [Decision::Recovered { gave_up: false, .. }]));
    assert_eq!(journal.watchdog["linux-01-a"].down_since, None);
    // Down again: the next attempt is the second in the window, after five minutes.
    assert!(round(&mut journal, 10).is_empty());
    assert!(round(&mut journal, 14).is_empty());
    assert_eq!(round(&mut journal, 15), ["restart linux-01-a #2"]);
    // Six hours after the first attempt it has aged out of the window.
    let late = start() + Duration::hours(7);
    plan(&running_view(late), &mut journal, late);
    assert_eq!(journal.watchdog.get("linux-01-a").map(|s| s.attempts.len()), None);
}

fn running_view(at: DateTime<Utc>) -> FleetView {
    let mut v = view(at);
    set_service(linux(&mut v), "active");
    v
}

/// A named change to the fleet view.
type Case = (&'static str, fn(&mut FleetView));

#[test]
fn watchdog_leaves_busy_drained_unknown_and_demo_runners_alone() {
    let cases: [Case; 6] = [
        ("busy", |v| linux(v).github.as_mut().unwrap().busy = true),
        ("online on GitHub", |v| {
            linux(v).github.as_mut().unwrap().status = "online".into()
        }),
        ("stale GitHub data", |v| linux(v).github.as_mut().unwrap().stale = true),
        ("stale SSH data", |v| linux(v).local.as_mut().unwrap().stale = true),
        ("unknown service", |v| set_service(linux(v), "unknown")),
        ("demo data", |v| v.source = DataSource::Demo),
    ];
    for (case, change) in cases {
        let mut journal = Journal::default();
        for minutes in [0, 2, 10, 100] {
            let at = start() + Duration::minutes(minutes);
            let mut v = view(at);
            change(&mut v);
            assert!(plan(&v, &mut journal, at).is_empty(), "{case}");
        }
    }
    let mut journal = Journal::default();
    journal.drains.insert(
        "linux-01-a".into(),
        DrainMark {
            runner: "linux-01-a".into(),
            host: "build-linux-01".into(),
            path: "~/gh-runners/acme-demo/runner-01".into(),
            registration: 301,
            review: "review".into(),
            requested_at: start(),
            state: DrainState::Stopped,
            stopped_at: Some(start()),
            last_error: None,
            checked_at: None,
        },
    );
    assert!(round(&mut journal, 0).is_empty());
    assert!(
        round(&mut journal, 10).is_empty(),
        "a drained runner is never restarted"
    );
}

#[test]
fn availability_counts_online_runners_per_capability_label() {
    let at = start();
    let mut v = view(at);
    let availability = publish::availability(&v, at).unwrap();
    assert_eq!(
        availability.online.get("linux-build"),
        Some(&0),
        "linux-01-a is offline"
    );
    assert!(availability.online.get("macos-build").is_some_and(|n| *n > 0));
    assert!(!availability.online.contains_key("fleet"));
    assert!(!availability.healthy);
    let value: serde_json::Value = serde_json::from_str(&availability.value(at)).unwrap();
    assert_eq!(value["updatedAt"], "2026-10-08T12:00:00Z");
    assert_eq!(value["online"]["linux-build"], 0);
    // The fingerprint ignores the time, so an unchanged fleet isn't written again.
    assert!(!availability.fingerprint().contains("updatedAt"));

    v.orgs[0].runners_as_of = Some(at - Duration::minutes(10));
    assert_eq!(
        publish::availability(&v, at).unwrap_err(),
        "Waiting for fresh GitHub runner data."
    );
}

#[test]
fn variable_names_follow_github_rules() {
    let named = |variable: &str| publish::PublishSettings {
        enabled: true,
        variable: variable.into(),
        interval_minutes: 5,
    };
    assert!(publish::check_settings(&named("RUNNER_FLEET_STATUS")).is_ok());
    for bad in ["", "1STATUS", "GITHUB_STATUS", "FLEET-STATUS", "FLEET STATUS"] {
        assert!(publish::check_settings(&named(bad)).is_err(), "{bad}");
    }
}

/// Answers `gh api` like GitHub: the variable doesn't exist until it is created.
#[derive(Default)]
struct FakeGh {
    created: Mutex<bool>,
    calls: Mutex<Vec<(Vec<String>, String)>>,
}

impl CommandRunner for FakeGh {
    fn run(
        &self,
        command: &[String],
        input: Option<&str>,
        _: StdDuration,
        _: NonZeroExit,
    ) -> Result<CommandResult, CommandError> {
        self.calls
            .locked()
            .push((command.to_vec(), input.unwrap_or_default().to_string()));
        let mut created = self.created.locked();
        let method = command[3].as_str();
        Ok(match (method, *created) {
            ("PATCH", false) => CommandResult {
                stdout: String::new(),
                stderr: "gh: Not Found (HTTP 404)".into(),
                returncode: 1,
            },
            ("POST", _) => {
                *created = true;
                CommandResult::ok("")
            }
            _ => CommandResult::ok(""),
        })
    }
}

#[test]
fn variable_is_created_once_then_updated_with_the_value_on_stdin() {
    let gh = Arc::new(FakeGh::default());
    let variables = GhVariables::new(gh.clone(), "gh", StdDuration::from_secs(5));
    variables
        .write("acme", "RUNNER_FLEET_STATUS", "{\"healthy\":true}")
        .unwrap();
    variables
        .write("acme", "RUNNER_FLEET_STATUS", "{\"healthy\":false}")
        .unwrap();
    let calls = gh.calls.locked().clone();
    let endpoints: Vec<String> = calls.iter().map(|(c, _)| c[3..5].join(" ")).collect();
    assert_eq!(
        endpoints,
        [
            "PATCH orgs/acme/actions/variables/RUNNER_FLEET_STATUS",
            "POST orgs/acme/actions/variables",
            "PATCH orgs/acme/actions/variables/RUNNER_FLEET_STATUS",
        ]
    );
    let create: serde_json::Value = serde_json::from_str(&calls[1].1).unwrap();
    assert_eq!(create["visibility"], "private");
    assert_eq!(create["value"], "{\"healthy\":true}");
    // The value never appears on the command line.
    assert!(calls.iter().all(|(c, _)| !c.iter().any(|a| a.contains("healthy"))));
}
