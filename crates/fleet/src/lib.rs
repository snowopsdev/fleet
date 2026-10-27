//! Runner Dashboard core, independent of the UI shell.
//!
//! - `github`, `gh`, `tailscale`, `machine`: read-only data sources
//! - `engine`: config, secrets, polling and user actions
//! - `derive`: pure `(config, observations, plan inputs, now, source) -> FleetView`
//! - `issues`: the attention findings attached to each view
//! - `monitoring`: local machine history, alerts and notification requests
//! - `jobs`: completed-job history, utilization analytics and the optional webhook job feed
//! - `desired`, `fleet_plan`: the inventory, its checks and reviewed changes
//! - `local_api`: the read-only loopback HTTP API
//! - `runner_version`: runner versions compared with the latest `actions/runner` release
//! - `view_updates`: view revisions, and when to push a re-derived view to the UI
//! - `demo`, `demo_desired`: a fictional dataset run through the same `derive`

mod atomic_write;
mod cli;
pub mod demo;
pub mod demo_desired;
pub mod derive;
pub mod desired;
pub mod engine;
pub mod fleet_plan;
pub mod gh;
pub mod github;
pub mod issues;
pub mod jobs;
pub mod labels;
pub mod local_api;
pub mod machine;
pub mod machine_diagnostics;
pub mod model;
pub mod monitoring;
pub mod runner_version;
pub mod ssh_reuse;
pub mod sync;
pub mod tailscale;
pub mod view_updates;
pub mod wake;

#[cfg(test)]
mod derive_tests;
#[cfg(test)]
mod test_server;
