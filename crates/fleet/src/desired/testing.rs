//! Fakes for GitHub, the worker hosts and subprocesses, shared by the unit and
//! integration tests. Nothing here contacts a real host or organization.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::github::{FleetGithub, GithubError, ReleaseAsset};
use super::maintenance::{CleanupCandidate, CleanupOptions, CleanupOutcome, CleanupTarget, JOB_RUNNING};
use super::model::{CheckStatus, DoctorCheck, GithubRunner, Host, LocalRunner, Runner, RunnerRole};
use super::process::{CommandError, CommandResult, CommandRunner, NonZeroExit};
use super::remote::{CleanupSurvey, FleetRemote, InstallRequest, RemoteError, Transient};
use crate::sync::MutexExt;

/// The registration token the fake GitHub mints.
pub const REGISTRATION_TOKEN: &str = "short-lived-registration";
/// The removal token the fake GitHub mints.
pub const REMOVAL_TOKEN: &str = "short-lived-removal";
/// The id the fake GitHub gives a runner a fake host installs.
pub const INSTALLED_ID: i64 = 44;

/// What the fake GitHub and hosts know, and what was asked of them.
#[derive(Debug)]
pub struct World {
    pub github: BTreeMap<String, GithubRunner>,
    /// Every host's runner directories; a probe reports its own host's.
    pub local: BTreeMap<String, LocalRunner>,
    /// Changes, in order: `install HOST RUNNER token=…`, `labels ID L1,L2`,
    /// `group GROUP ID`, `start`/`restart HOST RUNNER`, `retire HOST LEGACY`,
    /// `decommission HOST RUNNER token=…`, `delete ID`. Passwords are appended
    /// when supplied.
    pub calls: Vec<String>,
    /// Tokens minted, in order: `registration` or `removal`.
    pub tokens: Vec<String>,
    /// Read-only host calls, in order: `probe HOST`, `logs …`, `doctor HOST`.
    pub reads: Vec<String>,
    /// A probe of one of these hosts fails with its message.
    pub host_errors: BTreeMap<String, String>,
    /// Replaces the default log text.
    pub log_output: Option<String>,
    /// The trusted runner group's id.
    pub group_id: i64,
    /// Deregistered runners stay registered on GitHub.
    pub survives_removal: bool,
    /// What a cleanup survey finds on every host (filtered to the host's runners).
    pub cleanup_targets: Vec<CleanupTarget>,
    /// Runners with a job running on the host itself (stop and cleanup refuse them).
    pub jobs_on_host: Vec<String>,
    /// The runner release the fake GitHub publishes.
    pub latest_version: String,
    /// Upgrading one of these runners fails on the host.
    pub upgrade_failures: Vec<String>,
    /// Starting one of these runners fails on the host.
    pub start_failures: Vec<String>,
    /// The connection drops during a cleanup (after the host started removing files).
    pub cleanup_lost: bool,
    /// Stopping one of these runners stops it but reports an error.
    pub stop_errors_after_stopping: Vec<String>,
}

impl Default for World {
    fn default() -> Self {
        Self {
            github: BTreeMap::new(),
            local: BTreeMap::new(),
            calls: Vec::new(),
            tokens: Vec::new(),
            reads: Vec::new(),
            host_errors: BTreeMap::new(),
            log_output: None,
            group_id: 5,
            survives_removal: false,
            cleanup_targets: Vec::new(),
            jobs_on_host: Vec::new(),
            latest_version: "2.999.0".into(),
            upgrade_failures: Vec::new(),
            start_failures: Vec::new(),
            cleanup_lost: false,
            stop_errors_after_stopping: Vec::new(),
        }
    }
}

/// A fake GitHub organization and its worker hosts, sharing one `World`.
#[derive(Debug, Clone, Default)]
pub struct FakeFleet(Arc<Mutex<World>>);

impl FakeFleet {
    pub fn new(github: Vec<GithubRunner>, local: Vec<LocalRunner>) -> Self {
        let fleet = Self::default();
        {
            let mut world = fleet.world();
            world.github = github.into_iter().map(|g| (g.name.clone(), g)).collect();
            world.local = local.into_iter().map(|l| (l.name.clone(), l)).collect();
        }
        fleet
    }

    pub fn world(&self) -> MutexGuard<'_, World> {
        self.0.locked()
    }

    fn call(&self, call: String) {
        self.world().calls.push(call);
    }
}

/// ` NAME=PASSWORD` when the password was supplied.
fn password(name: &str, value: &Transient) -> String {
    value.get().map(|v| format!(" {name}={v}")).unwrap_or_default()
}

impl FleetGithub for FakeFleet {
    fn auth_status(&self) -> Result<String, GithubError> {
        Ok("ok".into())
    }
    fn list_runners(&self, _: &str) -> Result<BTreeMap<String, GithubRunner>, GithubError> {
        Ok(self.world().github.clone())
    }
    fn list_repository_runners(&self, _: &str) -> Result<BTreeMap<String, GithubRunner>, GithubError> {
        Ok(BTreeMap::new())
    }
    fn validate_runner_group_policy(&self, _: &str) -> Result<i64, GithubError> {
        Ok(self.world().group_id)
    }
    fn latest_asset(&self, _: &str) -> Result<ReleaseAsset, GithubError> {
        Ok(ReleaseAsset {
            version: self.world().latest_version.clone(),
            name: "runner.tar.gz".into(),
            url: "https://example.invalid".into(),
            sha256: Some("a".repeat(64)),
        })
    }
    fn registration_token(&self) -> Result<String, GithubError> {
        self.world().tokens.push("registration".into());
        Ok(REGISTRATION_TOKEN.into())
    }
    fn removal_token(&self, _: Option<&str>) -> Result<String, GithubError> {
        self.world().tokens.push("removal".into());
        Ok(REMOVAL_TOKEN.into())
    }
    fn set_labels(&self, id: i64, labels: &[String]) -> Result<(), GithubError> {
        let mut world = self.world();
        world.calls.push(format!("labels {id} {}", labels.join(",")));
        for runner in world.github.values_mut().filter(|r| r.id == id) {
            runner.labels = labels.iter().cloned().chain(["self-hosted".to_string()]).collect();
            runner.custom_labels = labels.iter().cloned().collect();
        }
        Ok(())
    }
    fn assign_group(&self, group: i64, id: i64) -> Result<(), GithubError> {
        let mut world = self.world();
        world.calls.push(format!("group {group} {id}"));
        for runner in world.github.values_mut().filter(|r| r.id == id) {
            runner.in_desired_group = Some(true);
        }
        Ok(())
    }
    fn delete_runner(&self, id: i64) -> Result<(), GithubError> {
        let mut world = self.world();
        world.calls.push(format!("delete {id}"));
        world.github.retain(|_, r| r.id != id);
        Ok(())
    }
}

impl FleetRemote for FakeFleet {
    fn probe(&self, host: &Host) -> Result<BTreeMap<String, LocalRunner>, RemoteError> {
        let mut world = self.world();
        world.reads.push(format!("probe {}", host.id));
        if let Some(error) = world.host_errors.get(&host.id) {
            return Err(RemoteError(error.clone()));
        }
        let names: Vec<&str> = host.runner_names().collect();
        Ok(world
            .local
            .iter()
            .filter(|(name, _)| names.contains(&name.as_str()))
            .map(|(name, runner)| (name.clone(), runner.clone()))
            .collect())
    }
    fn install(&self, host: &Host, runner: &Runner, request: &InstallRequest) -> Result<String, RemoteError> {
        let mut world = self.world();
        world.calls.push(format!(
            "install {} {} token={}{}{}",
            host.id,
            runner.name,
            request.token,
            password("service-password", request.service_password),
            password("sudo-password", request.sudo_password),
        ));
        world.github.insert(
            runner.name.clone(),
            GithubRunner::new(
                INSTALLED_ID,
                &runner.name,
                "online",
                false,
                &["self-hosted"],
                Some(false),
            ),
        );
        world.local.insert(
            runner.name.clone(),
            LocalRunner::new(&runner.name, &host.runner_path(runner), true, "inactive", None),
        );
        Ok(String::new())
    }
    fn retire(&self, host: &Host, runner: &Runner, _: &str, sudo: &Transient) -> Result<String, RemoteError> {
        let legacy = runner
            .replaces
            .as_ref()
            .ok_or_else(|| RemoteError(format!("{} does not replace a legacy runner", runner.name)))?;
        let mut world = self.world();
        world.calls.push(format!(
            "retire {} {}{}",
            host.id,
            legacy.name,
            password("sudo-password", sudo)
        ));
        if !world.survives_removal {
            world.github.remove(&legacy.name);
        }
        Ok(String::new())
    }
    fn decommission(&self, host: &Host, runner: &Runner, token: &str, sudo: &Transient) -> Result<String, RemoteError> {
        let mut world = self.world();
        world.calls.push(format!(
            "decommission {} {} token={token}{}",
            host.id,
            runner.name,
            password("sudo-password", sudo)
        ));
        if !world.survives_removal {
            world.github.remove(&runner.name);
        }
        Ok(String::new())
    }
    fn start(&self, host: &Host, runner: &Runner, _: &Transient) -> Result<String, RemoteError> {
        let mut world = self.world();
        if world.start_failures.contains(&runner.name) {
            return Err(RemoteError(format!("{} failed to start", runner.name)));
        }
        world.calls.push(format!("start {} {}", host.id, runner.name));
        if let Some(local) = world.local.get_mut(&runner.name) {
            local.service_state = "active".into();
        }
        if let Some(github) = world.github.get_mut(&runner.name) {
            github.status = "online".into();
        }
        Ok(String::new())
    }
    fn restart(&self, host: &Host, runner: &Runner, _: RunnerRole, _: &Transient) -> Result<String, RemoteError> {
        self.call(format!("restart {} {}", host.id, runner.name));
        Ok(String::new())
    }
    fn logs(&self, host: &Host, runner: &Runner, role: RunnerRole, lines: u32) -> Result<String, RemoteError> {
        let mut world = self.world();
        world
            .reads
            .push(format!("logs {} {} {role:?} {lines}", host.id, runner.name));
        Ok(world
            .log_output
            .clone()
            .unwrap_or_else(|| format!("==> {} last {lines} <==\n", runner.name)))
    }
    fn doctor(&self, host: &Host) -> Vec<DoctorCheck> {
        self.world().reads.push(format!("doctor {}", host.id));
        vec![DoctorCheck::new(&host.id, "ssh", CheckStatus::Pass, "connected")]
    }
    fn stop(&self, host: &Host, runner: &Runner, sudo: &Transient) -> Result<String, RemoteError> {
        let mut world = self.world();
        if world.jobs_on_host.contains(&runner.name) {
            return Err(RemoteError(format!("{JOB_RUNNING} in the runner directory")));
        }
        world.calls.push(format!(
            "stop {} {}{}",
            host.id,
            runner.name,
            password("sudo-password", sudo)
        ));
        if let Some(local) = world.local.get_mut(&runner.name) {
            local.service_state = "inactive".into();
        }
        if let Some(github) = world.github.get_mut(&runner.name) {
            github.status = "offline".into();
        }
        if world.stop_errors_after_stopping.contains(&runner.name) {
            return Err(RemoteError("svc.sh stop exited 1".into()));
        }
        Ok(String::new())
    }
    fn survey_cleanup(
        &self,
        host: &Host,
        candidates: &[CleanupCandidate],
        _: &CleanupOptions,
    ) -> Result<CleanupSurvey, RemoteError> {
        let mut world = self.world();
        world.reads.push(format!("survey {}", host.id));
        if let Some(error) = world.host_errors.get(&host.id) {
            return Err(RemoteError(error.clone()));
        }
        let targets = world
            .cleanup_targets
            .iter()
            .filter(|t| {
                candidates
                    .iter()
                    .any(|c| c.kind == t.kind && c.runner.as_ref().map(|(n, _)| n) == t.runner.as_ref())
            })
            .cloned()
            .collect();
        Ok(CleanupSurvey {
            targets,
            refused: Vec::new(),
        })
    }
    fn cleanup(
        &self,
        host: &Host,
        targets: &[CleanupTarget],
        _: &CleanupOptions,
    ) -> Result<Vec<CleanupOutcome>, RemoteError> {
        let mut world = self.world();
        if world.cleanup_lost {
            world.calls.push(format!("cleanup {} (connection lost)", host.id));
            return Err(RemoteError("ssh: connection timed out".into()));
        }
        let mut outcomes = Vec::new();
        let names: Vec<&str> = host.runner_names().collect();
        let active = |world: &World, name: &str| world.local.get(name).is_some_and(LocalRunner::service_active);
        for target in targets {
            let busy = match &target.runner {
                Some(name) => world.jobs_on_host.contains(name),
                None => !world.jobs_on_host.is_empty(),
            };
            // Like the host script: a runner's own service must be stopped
            // first, and shared caches need every runner on the host stopped.
            let running = match &target.runner {
                Some(name) => active(&world, name),
                None => names.iter().any(|n| active(&world, n)),
            };
            let path = target.path.clone().unwrap_or_default();
            if busy || running {
                let why = if busy {
                    JOB_RUNNING
                } else {
                    "a service is still running"
                };
                outcomes.push(CleanupOutcome {
                    kind: target.kind,
                    runner: target.runner.clone(),
                    message: format!("{why} in {path}; left it alone"),
                    ok: false,
                });
                continue;
            }
            world.calls.push(format!(
                "cleanup {} {} {}",
                host.id,
                target.kind.as_str(),
                target.path.as_deref().unwrap_or("-")
            ));
            outcomes.push(CleanupOutcome {
                kind: target.kind,
                runner: target.runner.clone(),
                message: format!("emptied {path}"),
                ok: true,
            });
        }
        world.cleanup_targets.retain(|t| !targets.contains(t));
        Ok(outcomes)
    }
    fn upgrade(&self, host: &Host, runner: &Runner, asset: &ReleaseAsset) -> Result<String, RemoteError> {
        let mut world = self.world();
        world
            .calls
            .push(format!("upgrade {} {} {}", host.id, runner.name, asset.version));
        if world.upgrade_failures.contains(&runner.name) {
            return Err(RemoteError("SHA-256 mismatch for runner.tar.gz".into()));
        }
        if let Some(local) = world.local.get_mut(&runner.name) {
            local.version = Some(asset.version.clone());
        }
        Ok(String::new())
    }
}

/// Operations wait between GitHub re-reads; tests don't.
pub fn no_sleep(_: Duration) {}

/// One program run through a `RecordingRunner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub command: Vec<String>,
    pub input: Option<String>,
    pub timeout: Duration,
}

/// The stdout a `RecordingRunner` answers a command line with.
type Respond = dyn Fn(&[String]) -> String + Send + Sync;

/// Records every program it's asked to run and answers with `respond`'s stdout.
pub struct RecordingRunner {
    runs: Mutex<Vec<Recorded>>,
    respond: Box<Respond>,
}

impl std::fmt::Debug for RecordingRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingRunner")
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

impl RecordingRunner {
    pub fn new(respond: impl Fn(&[String]) -> String + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            runs: Mutex::default(),
            respond: Box::new(respond),
        })
    }

    /// A runner whose programs succeed with no output.
    pub fn silent() -> Arc<Self> {
        Self::new(|_| String::new())
    }

    pub fn runs(&self) -> Vec<Recorded> {
        self.runs.locked().clone()
    }

    /// The most recent run.
    pub fn last(&self) -> Option<Recorded> {
        self.runs.locked().last().cloned()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(
        &self,
        command: &[String],
        input: Option<&str>,
        timeout: Duration,
        _: NonZeroExit,
    ) -> Result<CommandResult, CommandError> {
        self.runs.locked().push(Recorded {
            command: command.to_vec(),
            input: input.map(str::to_string),
            timeout,
        });
        Ok(CommandResult::ok((self.respond)(command)))
    }
}
