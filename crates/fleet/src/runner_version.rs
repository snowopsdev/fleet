//! Runner version drift: how far a runner's `actions/runner` version is behind the latest release.
//!
//! GitHub stops queueing jobs on a self-hosted runner that hasn't installed an available
//! update within 30 days of its release. Runners normally update themselves, so a runner that
//! is behind briefly is expected; one more than a minor release behind, or missing a release
//! published over 30 days ago, needs a manual update. Ages count from the oldest release the
//! runner is missing, not the latest one.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::RunnerRelease;

/// After this, GitHub may stop sending jobs to runners that haven't updated.
pub const UPDATE_DEADLINE: Duration = Duration::days(30);
/// Runners that update themselves have usually done so by now, so a runner still missing
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
    /// When the oldest release this runner is missing was published (the earliest of those
    /// read, for a runner older than all of them).
    pub missing_since: Option<DateTime<Utc>>,
    /// Minor releases between `current` and `latest`; None when the major versions differ.
    pub minor_versions_behind: Option<u32>,
    /// Behind a different major version or more than one minor release, or missing a release
    /// for over 30 days: GitHub may stop queueing jobs on it.
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

/// The highest parseable version among `releases`.
pub(crate) fn latest(releases: &[RunnerRelease]) -> Option<&RunnerRelease> {
    releases
        .iter()
        .filter_map(|r| Some((parse(&r.version)?, r)))
        .max_by_key(|(version, _)| *version)
        .map(|(_, r)| r)
}

/// Compare `current` with `releases` (any order); None when `current` or every release
/// version can't be parsed.
pub(crate) fn status(current: &str, releases: &[RunnerRelease], now: DateTime<Utc>) -> Option<RunnerVersionStatus> {
    let have = parse(current)?;
    let parsed: Vec<((u64, u64, u64), &RunnerRelease)> =
        releases.iter().filter_map(|r| Some((parse(&r.version)?, r))).collect();
    let (latest, release) = parsed.iter().max_by_key(|(version, _)| *version)?;
    let update_available = have < *latest;
    let missing_since = parsed
        .iter()
        .filter(|(version, _)| *version > have)
        .min_by_key(|(version, _)| *version)
        .and_then(|(_, r)| r.published_at);
    let minor_versions_behind =
        (have.0 == latest.0).then(|| u32::try_from(latest.1.saturating_sub(have.1)).unwrap_or(u32::MAX));
    let missed_deadline = missing_since.is_some_and(|at| now - at > UPDATE_DEADLINE);
    Some(RunnerVersionStatus {
        current: current.trim().to_string(),
        latest: release.version.clone(),
        latest_published_at: release.published_at,
        update_available,
        missing_since: missing_since.filter(|_| update_available),
        minor_versions_behind,
        overdue: update_available && (minor_versions_behind.is_none_or(|n| n > 1) || missed_deadline),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap()
    }

    /// Releases as (version, days since publishing), newest first like GitHub's list.
    fn releases(list: &[(&str, i64)]) -> Vec<RunnerRelease> {
        list.iter()
            .map(|(version, days)| RunnerRelease {
                version: (*version).into(),
                published_at: Some(now() - Duration::days(*days)),
            })
            .collect()
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
        let list = releases(&[("2.330.0", 3), ("2.329.0", 40)]);
        assert_eq!(
            status("2.329.0", &list, now()).unwrap(),
            RunnerVersionStatus {
                current: "2.329.0".into(),
                latest: "2.330.0".into(),
                latest_published_at: list[0].published_at,
                update_available: true,
                missing_since: list[0].published_at,
                minor_versions_behind: Some(1),
                overdue: false,
            }
        );
    }

    #[test]
    fn age_counts_from_the_oldest_missing_release() {
        // 2.330.0 came out 40 days ago; the newer patch is only 3 days old.
        let list = releases(&[("2.330.1", 3), ("2.330.0", 40), ("2.329.0", 70)]);
        let behind = status("2.329.0", &list, now()).unwrap();
        assert_eq!(behind.latest, "2.330.1");
        assert_eq!(behind.missing_since, Some(now() - Duration::days(40)));
        assert!(behind.overdue);
        let patch_behind = status("2.330.0", &list, now()).unwrap();
        assert_eq!(patch_behind.missing_since, Some(now() - Duration::days(3)));
        assert!(patch_behind.update_available && !patch_behind.overdue);
    }

    #[test]
    fn overdue_when_far_behind_or_on_another_major_version() {
        let list = releases(&[("2.330.0", 3)]);
        assert!(status("2.328.2", &list, now()).unwrap().overdue);
        assert!(status("1.330.0", &list, now()).unwrap().overdue);
        let old = releases(&[("2.330.0", 31)]);
        assert!(status("2.330.0-pre", &old, now()).is_some_and(|s| !s.update_available && !s.overdue));
        assert!(status("2.329.9", &old, now()).unwrap().overdue);
    }

    #[test]
    fn current_or_newer_needs_nothing() {
        let list = releases(&[("2.330.0", 90), ("bogus", 1)]);
        for current in ["2.330.0", "2.331.0"] {
            let status = status(current, &list, now()).unwrap();
            assert!(!status.update_available && !status.overdue && status.missing_since.is_none());
        }
        assert_eq!(status("unknown", &list, now()), None);
        assert_eq!(status("2.330.0", &[], now()), None);
    }
}
