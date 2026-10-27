//! Runner version drift: how far a runner's `actions/runner` version is behind the latest release.
//!
//! GitHub stops queueing jobs on a self-hosted runner that hasn't installed an available
//! update within 30 days of its release. Runners normally update themselves, so a runner that
//! is behind briefly is expected; one more than a minor release behind, or behind a release
//! older than 30 days, needs a manual update.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::RunnerRelease;

/// After this, GitHub may stop sending jobs to runners that haven't updated.
pub const UPDATE_DEADLINE: Duration = Duration::days(30);
/// Runners that update themselves have usually done so by now, so a runner still behind
/// a release this old is worth a finding.
pub const UPDATE_GRACE: Duration = Duration::days(7);

/// A runner's version compared with the latest `actions/runner` release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub struct RunnerVersionStatus {
    /// From GitHub's runner list, or the machine when GitHub doesn't report it.
    pub current: String,
    pub latest: String,
    pub latest_published_at: Option<DateTime<Utc>>,
    pub update_available: bool,
    /// Minor releases between `current` and `latest`; None when the major versions differ.
    pub minor_versions_behind: Option<u32>,
    /// Behind a different major version, more than one minor release, or a release older
    /// than 30 days: GitHub may stop queueing jobs on it.
    pub overdue: bool,
}

/// `major.minor.patch`, ignoring a leading `v` and any pre-release or build suffix.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next().unwrap_or(Some(0))?);
    parts.next().is_none().then_some(version)
}

/// Compare `current` with `release`; None when either version can't be parsed.
pub(crate) fn status(current: &str, release: &RunnerRelease, now: DateTime<Utc>) -> Option<RunnerVersionStatus> {
    let have = parse(current)?;
    let latest = parse(&release.version)?;
    let update_available = have < latest;
    let minor_versions_behind =
        (have.0 == latest.0).then(|| u32::try_from(latest.1.saturating_sub(have.1)).unwrap_or(u32::MAX));
    let old_release = release.published_at.is_some_and(|at| now - at > UPDATE_DEADLINE);
    Some(RunnerVersionStatus {
        current: current.trim().to_string(),
        latest: release.version.clone(),
        latest_published_at: release.published_at,
        update_available,
        minor_versions_behind,
        overdue: update_available && (minor_versions_behind.is_none_or(|n| n > 1) || old_release),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn release(version: &str, days_old: i64) -> (RunnerRelease, DateTime<Utc>) {
        let now = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
        (
            RunnerRelease {
                version: version.into(),
                published_at: Some(now - Duration::days(days_old)),
            },
            now,
        )
    }

    #[test]
    fn parses_tags_and_suffixes() {
        assert_eq!(parse("v2.329.0"), Some((2, 329, 0)));
        assert_eq!(parse("2.330.1-rc1"), Some((2, 330, 1)));
        assert_eq!(parse("2.330"), Some((2, 330, 0)));
        assert_eq!(parse("2.x.0"), None);
        assert_eq!(parse("2.1.0.4"), None);
    }

    #[test]
    fn one_minor_behind_a_recent_release_is_an_update_not_overdue() {
        let (latest, now) = release("2.330.0", 3);
        let status = status("2.329.0", &latest, now).unwrap();
        assert_eq!(
            status,
            RunnerVersionStatus {
                current: "2.329.0".into(),
                latest: "2.330.0".into(),
                latest_published_at: latest.published_at,
                update_available: true,
                minor_versions_behind: Some(1),
                overdue: false,
            }
        );
    }

    #[test]
    fn overdue_when_far_behind_or_the_release_is_old() {
        let (recent, now) = release("2.330.0", 3);
        assert!(status("2.328.2", &recent, now).unwrap().overdue);
        assert!(status("1.330.0", &recent, now).unwrap().overdue);
        let (old, now) = release("2.330.0", 31);
        assert!(status("2.330.0-pre", &old, now).is_some_and(|s| !s.update_available && !s.overdue));
        assert!(status("2.329.9", &old, now).unwrap().overdue);
    }

    #[test]
    fn current_or_newer_needs_nothing() {
        let (latest, now) = release("2.330.0", 90);
        for current in ["2.330.0", "2.331.0"] {
            let status = status(current, &latest, now).unwrap();
            assert!(!status.update_available && !status.overdue);
        }
        assert_eq!(status("unknown", &latest, now), None);
    }
}
