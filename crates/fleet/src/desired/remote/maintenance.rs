//! Scripts for reviewed maintenance: stopping a runner's service when no job
//! runs in its directory, surveying and removing cleanup targets, and
//! replacing a stopped runner's `bin/` and `externals/` with a verified
//! release. Values travel on stdin like every other worker script.

use std::time::Duration;

use super::super::github::ReleaseAsset;
use super::super::maintenance::{CleanupCandidate, CleanupKind, CleanupOptions, CleanupOutcome, CleanupTarget};
use super::super::model::{Host, Runner};
use super::super::py::{ps_quote, sh_quote, sh_remote_path, splitlines};
use super::scripts::{RUN_AS_ROOT_SH, UPGRADE, WIN_FIND_SERVICE};
use super::{RemoteClient, RemoteError, Transient};

const CLEANUP_SH: &str = include_str!("../../../scripts/worker-cleanup.sh");
const CLEANUP_PS1: &str = include_str!("../../../scripts/worker-cleanup.ps1");

type Result<T> = std::result::Result<T, RemoteError>;

/// The cleanup script's own deadline: it starts no new removal after this
/// many seconds, comfortably inside the SSH timeout below, so a removal isn't
/// still running on the host after the controller gave up on it.
const SCRIPT_SECONDS: u64 = 840;
/// How long the controller waits for a cleanup before treating the host as lost.
const CLEAN_TIMEOUT_SECONDS: u64 = 960;

/// What a survey found: targets that exist, and ones the host refused to measure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupSurvey {
    pub targets: Vec<CleanupTarget>,
    /// Targets left alone, such as a symlinked `_work/_temp`, with the reason.
    pub refused: Vec<(CleanupKind, Option<String>, String)>,
}

/// Mode of the cleanup script: read-only, or removing reviewed targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Survey,
    Clean,
}

impl Mode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Survey => "survey",
            Self::Clean => "clean",
        }
    }
}

/// One call of the cleanup script: a kind, the runner and its exact directory
/// (runner targets), the path the review resolved, and diagnostic log names.
struct Call<'a> {
    kind: CleanupKind,
    runner: Option<(&'a str, &'a str)>,
    expected: &'a str,
    files: &'a [String],
}

fn cleanup_script(host: &Host, mode: Mode, options: &CleanupOptions, calls: &[Call]) -> String {
    if host.is_windows() {
        let mut out = format!(
            "$env:RF_MODE = {}\n$env:RF_DIAG_DAYS = {}\n$env:RF_SECONDS = '{SCRIPT_SECONDS}'\n{CLEANUP_PS1}\ntry {{\n",
            ps_quote(mode.as_str()),
            ps_quote(&options.diag_older_than_days.to_string())
        );
        for call in calls {
            // Windows hosts only have runner targets; machine-wide ones aren't offered.
            let Some((name, root)) = call.runner else { continue };
            let files: Vec<String> = call.files.iter().map(|f| ps_quote(f)).collect();
            out.push_str(&format!(
                "Invoke-RunnerTarget {} {} {} {} @({})\n",
                ps_quote(call.kind.as_str()),
                ps_quote(name),
                ps_quote(root),
                ps_quote(call.expected),
                files.join(", ")
            ));
        }
        out.push_str("} finally { Unlock-All }\n");
        return out;
    }
    let mut out = format!(
        "RF_MODE={}\nRF_DIAG_DAYS={}\nRF_SECONDS={SCRIPT_SECONDS}\n{CLEANUP_SH}\n",
        mode.as_str(),
        options.diag_older_than_days
    );
    for call in calls {
        let line = match call.runner {
            Some((name, root)) => {
                let mut line = format!(
                    "runner_target {} {} {} {}",
                    sh_quote(call.kind.as_str()),
                    sh_quote(name),
                    sh_remote_path(root),
                    sh_quote(call.expected)
                );
                for file in call.files {
                    line.push(' ');
                    line.push_str(&sh_quote(file));
                }
                line
            }
            None => format!(
                "machine_target {} {}",
                sh_quote(call.kind.as_str()),
                sh_quote(call.expected)
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("exit 0\n");
    out
}

/// The survey script for `candidates`.
pub(crate) fn survey_script(host: &Host, candidates: &[CleanupCandidate], options: &CleanupOptions) -> String {
    let calls: Vec<Call> = candidates
        .iter()
        .map(|c| Call {
            kind: c.kind,
            runner: c.runner.as_ref().map(|(n, p)| (n.as_str(), p.as_str())),
            expected: "",
            files: &[],
        })
        .collect();
    cleanup_script(host, Mode::Survey, options, &calls)
}

/// The script removing exactly `targets`, each at the path the review resolved.
pub(crate) fn clean_script(host: &Host, targets: &[CleanupTarget], options: &CleanupOptions) -> Result<String> {
    let mut paths = Vec::new();
    for target in targets {
        let root = match &target.runner {
            Some(name) => {
                let runner = host
                    .desired_runners()
                    .find(|r| &r.name == name)
                    .ok_or_else(|| RemoteError(format!("{name} is not a desired runner on {}", host.id)))?;
                Some((name.as_str(), host.runner_path(runner)))
            }
            None => None,
        };
        paths.push(root);
    }
    let calls: Vec<Call> = targets
        .iter()
        .zip(&paths)
        .map(|(t, root)| Call {
            kind: t.kind,
            runner: root.as_ref().map(|(n, p)| (*n, p.as_str())),
            expected: t.path.as_deref().unwrap_or(""),
            files: &t.files,
        })
        .collect();
    Ok(cleanup_script(host, Mode::Clean, options, &calls))
}

fn field(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn number(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

fn malformed(raw: &str) -> RemoteError {
    RemoteError(format!("malformed cleanup data: {}", super::super::py::repr(raw)))
}

fn kind(value: &str, raw: &str) -> Result<CleanupKind> {
    CleanupKind::parse(value).ok_or_else(|| malformed(raw))
}

/// Parses the survey's RFC, RFF and RFE rows. Log names (RFF) belong to the
/// diagnostic-log target of the same runner.
pub fn parse_survey(output: &str) -> Result<CleanupSurvey> {
    let mut survey = CleanupSurvey::default();
    let mut named: Vec<(CleanupKind, String, String)> = Vec::new();
    for raw in splitlines(output) {
        let raw = raw.trim_end_matches('\r');
        let fields: Vec<&str> = raw.split('\t').collect();
        match fields.first().copied() {
            Some("RFC") => {
                let [_, k, runner, path, kib, count, detail] = fields[..] else {
                    return Err(malformed(raw));
                };
                survey.targets.push(CleanupTarget {
                    kind: kind(k, raw)?,
                    runner: field(runner),
                    path: field(path),
                    kib: number(kib),
                    count: number(count),
                    detail: field(detail),
                    files: Vec::new(),
                });
            }
            Some("RFF") => {
                let [_, k, runner, file] = fields[..] else {
                    return Err(malformed(raw));
                };
                named.push((kind(k, raw)?, runner.to_string(), file.to_string()));
            }
            Some("RFE") => {
                let [_, k, runner, message] = fields[..] else {
                    return Err(malformed(raw));
                };
                survey.refused.push((kind(k, raw)?, field(runner), message.to_string()));
            }
            _ => {}
        }
    }
    for target in &mut survey.targets {
        target.files = named
            .iter()
            .filter(|(k, runner, _)| *k == target.kind && Some(runner) == target.runner.as_ref())
            .map(|(_, _, file)| file.clone())
            .collect();
    }
    Ok(survey)
}

/// A size in KiB for people: `512 KiB`, `12.3 MiB`, `1.5 GiB`.
#[expect(clippy::cast_precision_loss, reason = "sizes are shown to one decimal place")]
pub fn format_kib(kib: u64) -> String {
    const MIB: u64 = 1024;
    const GIB: u64 = 1024 * 1024;
    if kib >= GIB {
        format!("{:.1} GiB", kib as f64 / GIB as f64)
    } else if kib >= MIB {
        format!("{:.1} MiB", kib as f64 / MIB as f64)
    } else {
        format!("{kib} KiB")
    }
}

/// Parses the clean script's RFX (removed), RFE (refused or failed) and RFT
/// (stopped part-way at the host's deadline) rows.
pub fn parse_clean(output: &str) -> Result<Vec<CleanupOutcome>> {
    let mut outcomes = Vec::new();
    for raw in splitlines(output) {
        let raw = raw.trim_end_matches('\r');
        let fields: Vec<&str> = raw.split('\t').collect();
        match fields.first().copied() {
            Some("RFX") => {
                let [_, k, runner, path, kib, count, detail] = fields[..] else {
                    return Err(malformed(raw));
                };
                let kind = kind(k, raw)?;
                let freed = number(kib).map(format_kib).unwrap_or_default();
                let message = match kind {
                    CleanupKind::WorkTemp
                    | CleanupKind::WorkActions
                    | CleanupKind::Work
                    | CleanupKind::XcodeDerivedData => {
                        format!("emptied {path} ({freed})")
                    }
                    CleanupKind::Diag => format!(
                        "removed {} diagnostic log(s) from {path} ({freed})",
                        number(count).unwrap_or(0)
                    ),
                    CleanupKind::PreviousBin | CleanupKind::PreviousExternals => format!(
                        "removed {} previous runner version folder(s) from {path} ({freed})",
                        number(count).unwrap_or(0)
                    ),
                    CleanupKind::UpgradeLeftovers => format!(
                        "removed {} leftover upgrade staging folder(s) from {path} ({freed})",
                        number(count).unwrap_or(0)
                    ),
                    CleanupKind::UnavailableSimulators => {
                        format!("deleted {} unavailable simulator(s)", number(count).unwrap_or(0))
                    }
                    CleanupKind::Docker => format!("pruned Docker: {detail}"),
                };
                outcomes.push(CleanupOutcome {
                    kind,
                    runner: field(runner),
                    message,
                    ok: true,
                    timed_out: false,
                });
            }
            Some(tag @ ("RFE" | "RFT")) => {
                let [_, k, runner, message] = fields[..] else {
                    return Err(malformed(raw));
                };
                outcomes.push(CleanupOutcome {
                    kind: kind(k, raw)?,
                    runner: field(runner),
                    message: message.to_string(),
                    ok: false,
                    timed_out: tag == "RFT",
                });
            }
            _ => {}
        }
    }
    Ok(outcomes)
}

/// Stops one exact runner's service, refusing while a job runs in its
/// directory. A logon task is disabled too, so its repeating trigger can't
/// start it again until it's resumed.
pub(crate) fn stop_script(host: &Host, runner: &Runner, sudo_password: &Transient) -> String {
    let path = host.runner_path(runner);
    let name = &runner.name;
    let busy = super::super::maintenance::JOB_RUNNING;
    if host.is_windows() {
        // CIM attributes a job by its executable path or command line; one
        // whose neither can be read (another account, no elevation) counts as
        // this runner's. `$bin` also covers a self-updated `bin.VERSION`.
        let worker = format!(
            "$bin = Join-Path $path 'bin'\n\
$jobs = @(Get-CimInstance Win32_Process -Filter \"Name='Runner.Worker.exe'\" | Where-Object {{ $where = if ($_.ExecutablePath) {{ $_.ExecutablePath }} else {{ $_.CommandLine }}; -not $where -or $where.IndexOf($bin, [StringComparison]::OrdinalIgnoreCase) -ge 0 }})\n\
if ($jobs.Count -gt 0) {{ throw \"{busy} in $path (or a runner job whose path can't be read); refusing to stop {name}\" }}\n"
        );
        if host.is_windows_logon() {
            return format!(
                "$ErrorActionPreference = 'Stop'\n$path = {}\n$task = {}\n\
if ((Get-Content -LiteralPath (Join-Path $path '.fleet-task') -Raw).Trim() -ne $task) {{ throw \"logon task marker at $path does not name $task\" }}\n\
{worker}\
Disable-ScheduledTask -TaskName $task | Out-Null\n\
Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue\n\
Get-Process -Name Runner.Listener -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -and $_.Path.StartsWith($bin, [StringComparison]::OrdinalIgnoreCase) }} | Stop-Process -Force\n\
Write-Output \"stopped $task; it stays disabled until it is started again\"\n",
                ps_quote(&path),
                ps_quote(&format!("runner-fleet-{name}")),
            );
        }
        return format!(
            "$ErrorActionPreference = 'Stop'\n$path = {}\n{WIN_FIND_SERVICE}\
if (-not $service) {{ throw \"No Actions runner service found for exact path $path\" }}\n\
{worker}\
Stop-Service -Name $service.Name\n\
Write-Output \"stopped $($service.Name)\"\n",
            ps_quote(&path)
        );
    }
    let unit = sh_quote(&format!("runner-fleet-{name}.service"));
    let privilege = if host.uses_sudo() { "run_as_root " } else { "" };
    let sudo = if host.uses_sudo() {
        format!(
            "RF_SUDO_PASSWORD={}\n{RUN_AS_ROOT_SH}",
            sh_quote(sudo_password.or_empty())
        )
    } else {
        String::new()
    };
    format!(
        "set -euo pipefail\nroot={root}\n{sudo}\n\
test -x \"$root/svc.sh\" || {{ echo \"missing svc.sh at $root\" >&2; exit 1; }}\n\
cd \"$root\"\n\
real=\"$(pwd -P)\"\n\
processes=\"$(ps -A -ww -o command= 2>/dev/null || true)\"\n\
if printf '%s\\n' \"$processes\" | grep -F -- \"$real/bin\" | grep -F -- Runner.Worker >/dev/null; then\n\
  echo \"{busy} in $real; refusing to stop {shown}\" >&2\n\
  exit 5\n\
fi\n\
if [ -f /etc/NIXOS ]; then\n\
test \"$(cat .service)\" = {unit}\n\
run_as_root systemctl stop {unit}\n\
else\n\
{privilege}./svc.sh stop\n\
fi\n\
echo \"stopped {shown}\"\n",
        root = sh_remote_path(&path),
        shown = name,
    )
}

impl RemoteClient {
    pub(super) fn stop_runner(&self, host: &Host, runner: &Runner, sudo_password: &Transient) -> Result<String> {
        let timeout = if host.is_windows_logon() { 90 } else { 60 };
        self.run_script(
            host,
            &stop_script(host, runner, sudo_password),
            Some(Duration::from_secs(timeout)),
        )
    }

    pub(super) fn survey(
        &self,
        host: &Host,
        candidates: &[CleanupCandidate],
        options: &CleanupOptions,
    ) -> Result<CleanupSurvey> {
        let output = self.run_script(
            host,
            &survey_script(host, candidates, options),
            Some(Duration::from_secs(180)),
        )?;
        parse_survey(&output)
    }

    pub(super) fn clean(
        &self,
        host: &Host,
        targets: &[CleanupTarget],
        options: &CleanupOptions,
    ) -> Result<Vec<CleanupOutcome>> {
        let output = self.run_script(
            host,
            &clean_script(host, targets, options)?,
            Some(Duration::from_secs(CLEAN_TIMEOUT_SECONDS)),
        )?;
        parse_clean(&output)
    }

    pub(super) fn replace_binaries(&self, host: &Host, runner: &Runner, asset: &ReleaseAsset) -> Result<String> {
        let checksum = asset
            .sha256
            .clone()
            .ok_or_else(|| RemoteError(format!("no SHA-256 digest for {}", asset.name)))?;
        let values = [
            ("RF_ROOT", host.runner_path(runner)),
            ("RF_NAME", runner.name.clone()),
            ("RF_ASSET_URL", asset.url.clone()),
            ("RF_ASSET_NAME", asset.name.clone()),
            ("RF_CHECKSUM", checksum),
            ("RF_VERSION", asset.version.clone()),
        ];
        self.run_worker_script(host, &values, UPGRADE, Duration::from_secs(600))
    }
}
