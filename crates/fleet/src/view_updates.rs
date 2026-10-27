//! View revisions, and when a periodically re-derived view is worth pushing to the desktop UI.
//!
//! The revision fingerprints a view's content without `generatedAt`. Freshness flags such as
//! `stale` are content, so a view whose data crossed a staleness boundary has a new revision
//! and is pushed on the next re-derivation; ages themselves are computed by the UI's clock.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

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

/// Remembers the last pushed revision so unchanged periodic re-derivations aren't pushed,
/// except for a heartbeat that keeps "updated" times current.
#[derive(Debug, Default)]
pub struct ViewPushes {
    last: Option<(String, Instant)>,
}

impl ViewPushes {
    /// The longest an unchanged view goes without a push.
    pub const HEARTBEAT: Duration = Duration::from_secs(30);

    /// Whether a periodic re-derivation with `revision` should be pushed at `now`.
    pub fn due(&self, revision: &str, now: Instant) -> bool {
        self.last
            .as_ref()
            .is_none_or(|(last, at)| last != revision || now.saturating_duration_since(*at) >= Self::HEARTBEAT)
    }

    /// Record any push, periodic or not.
    pub fn pushed(&mut self, revision: String, now: Instant) {
        self.last = Some((revision, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::derive;
    use crate::model::DataSource;
    use chrono::{TimeZone, Utc};

    #[test]
    fn unchanged_views_wait_for_the_heartbeat() {
        let start = Instant::now();
        let mut pushes = ViewPushes::default();
        assert!(pushes.due("a", start));
        pushes.pushed("a".into(), start);
        assert!(!pushes.due("a", start + Duration::from_secs(5)));
        assert!(pushes.due("b", start + Duration::from_secs(5)));
        assert!(pushes.due("a", start + ViewPushes::HEARTBEAT));
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
