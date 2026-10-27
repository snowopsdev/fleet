//! Completed-job history and utilization analytics. Reads only the local journal (or
//! demo history in demo mode); never calls GitHub.

use axum::Extension;
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{ApiError, ApiState, Snapshot};
use super::queries::{self, Parameters};
use crate::jobs::analytics::FleetAnalyticsWindow;
use crate::jobs::history::{FleetJobHistoryFilter, MAX_JOB_PAGE, valid_cursor};
use crate::jobs::{FleetJobConclusion, JOB_RETENTION, MAX_JOBS};

type Reply = Result<Json<Value>, ApiError>;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct JobHistoryQuery {
    /// Machine ID, inventory alias or name; a retained history identity also works.
    pub machine: Option<String>,
    /// Runner key or name; names also match runners GitHub no longer lists.
    pub runner: Option<String>,
    /// Disambiguates runner names.
    pub org: Option<String>,
    /// `owner/name` or the repository name.
    pub repo: Option<String>,
    pub conclusion: Option<FleetJobConclusion>,
    /// Exclusive cursor: older jobs, newest first.
    pub before: Option<String>,
    /// Exclusive cursor: newer jobs, oldest first.
    pub after: Option<String>,
    pub limit: Option<u16>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AnalyticsQuery {
    #[serde(default)]
    pub window: FleetAnalyticsWindow,
}

pub(super) async fn history(
    State(state): State<ApiState>,
    Extension(s): Extension<Snapshot>,
    Parameters(q): Parameters<JobHistoryQuery>,
) -> Reply {
    if q.limit.is_some_and(|n| n == 0 || usize::from(n) > MAX_JOB_PAGE) {
        return Err(ApiError::invalid(format!(
            "limit must be between 1 and {MAX_JOB_PAGE}."
        )));
    }
    for cursor in [&q.before, &q.after].into_iter().flatten() {
        if !valid_cursor(cursor) {
            return Err(ApiError::invalid(
                "before and after must be 40-digit job cursors returned by this API.",
            ));
        }
    }
    let machine_key = match &q.machine {
        None => None,
        Some(id) => Some(match queries::resolve_machine(&s.view, id) {
            Ok(machine) => machine.preferences_key(),
            // A machine that's gone keeps its history under its retained identity.
            Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => id.clone(),
            Err(error) => return Err(error),
        }),
    };
    let (runner_key, runner_name) = match &q.runner {
        None => (None, None),
        Some(key) => match queries::resolve_runner(&s.view, key, q.org.as_deref()) {
            Ok(flat) => (Some(flat.runner.key), None),
            Err(error) if error.status == axum::http::StatusCode::NOT_FOUND => {
                if key.starts_with("gh:") {
                    (Some(key.clone()), None)
                } else {
                    (None, Some(key.clone()))
                }
            }
            Err(error) => return Err(error),
        },
    };
    let filter = FleetJobHistoryFilter {
        machine_key,
        runner_key,
        runner_name,
        repo: q.repo,
        conclusion: q.conclusion,
        before: q.before,
        after: q.after,
        limit: q.limit,
    };
    let engine = state.engine.clone();
    let view = s.view.clone();
    let page = tokio::task::spawn_blocking(move || engine.job_history(&view, &filter))
        .await
        .map_err(|e| ApiError::internal(format!("Couldn't read job history: {e}")))?;
    Ok(Json(json!({
        "jobs": page.jobs, "order": page.order, "hasMore": page.has_more, "nextCursor": page.next_cursor,
        "historySince": page.history_since, "storageError": page.storage_error,
        "retentionDays": JOB_RETENTION.num_days(), "maxJobs": MAX_JOBS,
    })))
}

pub(super) async fn analytics(
    State(state): State<ApiState>,
    Extension(s): Extension<Snapshot>,
    Parameters(q): Parameters<AnalyticsQuery>,
) -> Reply {
    let engine = state.engine.clone();
    let view = s.view.clone();
    let analytics = tokio::task::spawn_blocking(move || engine.job_analytics(&view, q.window))
        .await
        .map_err(|e| ApiError::internal(format!("Couldn't summarize job history: {e}")))?;
    Ok(Json(json!({
        "analytics": analytics, "historySince": s.view.activity.history_since,
        "storageError": s.view.activity.storage_error,
    })))
}
