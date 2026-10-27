//! Prints demo job history and analytics as JSON at the demo fixture's time, for the
//! browser-only preview's Activity page:
//!
//!     cargo run -q -p fleet --example demo_activity_json > src/fixtures/demo-activity.json

#![expect(clippy::unwrap_used, reason = "fixture generator: any failure should stop it loudly")]

use chrono::{TimeZone, Utc};
use fleet::derive::derive;
use fleet::jobs::analytics::{FleetAnalyticsWindow, summarize};
use fleet::jobs::history::FleetJobHistoryFilter;
use fleet::jobs::{FleetHostedRates, demo};
use fleet::model::DataSource;
use serde_json::json;

fn main() {
    let now = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
    let (config, obs) = fleet::demo::dataset(now);
    let view = derive(
        &config,
        &obs,
        &fleet::demo_desired::plan_inputs(now),
        now,
        DataSource::Demo,
    );
    let since = demo::history_since(now);
    let jobs = demo::history(&view, since, now);
    let (page, order, has_more, next_cursor) = FleetJobHistoryFilter {
        limit: Some(100),
        ..Default::default()
    }
    .page(&jobs);
    let rates = FleetHostedRates::default();
    let analytics = |window| summarize(&jobs, Some(since), &view, &rates, window, now);
    let fixture = json!({
        "generatedAt": now,
        "history": { "jobs": page, "order": order, "hasMore": has_more, "nextCursor": next_cursor,
            "historySince": since, "storageError": null },
        "analytics": {
            "24h": analytics(FleetAnalyticsWindow::Day),
            "7d": analytics(FleetAnalyticsWindow::Week),
            "30d": analytics(FleetAnalyticsWindow::Month),
        },
    });
    println!("{}", serde_json::to_string_pretty(&fixture).unwrap());
}
