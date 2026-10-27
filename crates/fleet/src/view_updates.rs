//! View revisions, and when a periodically re-derived view is worth pushing to the desktop UI.
//!
//! The revision fingerprints a view's content without `generatedAt`. Freshness flags such as
//! `stale` are content, so a view whose data crossed a staleness boundary has a new revision
//! and is pushed on the next re-derivation; ages themselves are computed by the UI's clock.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::model::FleetView;

/// An opaque fingerprint of `view`'s content, excluding `generatedAt`.
///
/// # Errors
///
/// Fails only if the view can't be serialized, which would be a bug in its types.
pub fn revision(view: &FleetView) -> Result<String, serde_json::Error> {
    let mut content = serde_json::to_value(view)?;
    if let Some(object) = content.as_object_mut() {
        object.remove("generatedAt");
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    content.to_string().hash(&mut hash);
    Ok(format!("{:016x}", hash.finish()))
}

/// The last view pushed to the UI.
#[derive(Debug)]
struct Pushed {
    revision: String,
    generated_at: DateTime<Utc>,
    at: Instant,
}

/// Remembers the last pushed view so unchanged periodic re-derivations aren't pushed, except
/// for a heartbeat that keeps "updated" times current, and so a periodic view derived before
/// the latest push never replaces it.
#[derive(Debug, Default)]
pub struct ViewPushes {
    last: Option<Pushed>,
}

impl ViewPushes {
    /// The longest an unchanged view goes without a push.
    pub const HEARTBEAT: Duration = Duration::from_secs(30);

    /// Whether a periodic re-derivation with `revision`, generated at `generated_at`, should be
    /// pushed at `now`. A view older than the last push is never due: a poll pushed newer data
    /// while this one was being recorded.
    pub fn due(&self, revision: &str, generated_at: DateTime<Utc>, now: Instant) -> bool {
        self.last.as_ref().is_none_or(|last| {
            generated_at >= last.generated_at
                && (last.revision != revision || now.saturating_duration_since(last.at) >= Self::HEARTBEAT)
        })
    }

    /// Record any push, periodic or not.
    pub fn pushed(&mut self, revision: String, generated_at: DateTime<Utc>, now: Instant) {
        self.last = Some(Pushed {
            revision,
            generated_at,
            at: now,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::derive;
    use crate::model::DataSource;
    use chrono::TimeZone;

    #[test]
    fn unchanged_views_wait_for_the_heartbeat() {
        let start = Instant::now();
        let at = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
        let mut pushes = ViewPushes::default();
        assert!(pushes.due("a", at, start));
        pushes.pushed("a".into(), at, start);
        assert!(!pushes.due("a", at, start + Duration::from_secs(5)));
        assert!(pushes.due("b", at, start + Duration::from_secs(5)));
        assert!(pushes.due("a", at, start + ViewPushes::HEARTBEAT));
    }

    #[test]
    fn a_periodic_view_older_than_the_last_push_is_dropped() {
        let start = Instant::now();
        let tick = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
        let mut pushes = ViewPushes::default();
        // A poll pushed a newer view while the tick's view was being recorded.
        pushes.pushed("poll".into(), tick + chrono::Duration::seconds(1), start);
        assert!(!pushes.due("tick", tick, start + Duration::from_secs(1)));
        assert!(!pushes.due("tick", tick, start + ViewPushes::HEARTBEAT));
        assert!(pushes.due(
            "next",
            tick + chrono::Duration::seconds(5),
            start + Duration::from_secs(5)
        ));
    }

    #[test]
    fn revision_ignores_generated_at_but_tracks_staleness() {
        let at = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
        let (config, obs) = crate::demo::dataset(at);
        let plan = crate::fleet_plan::FleetPlanInputs::default();
        let view = |seconds| {
            derive(
                &config,
                &obs,
                &plan,
                at + chrono::Duration::seconds(seconds),
                DataSource::Live,
            )
        };
        let first = revision(&view(0)).unwrap();
        assert_eq!(revision(&view(5)).unwrap(), first, "time alone doesn't change content");
        // The demo's GitHub runners were read 9 seconds before `at`; they go stale after 150.
        assert_ne!(revision(&view(150)).unwrap(), first);
    }
}
