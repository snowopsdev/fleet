//! Final job states for the completed-job history (repo "Actions: read").
//!
//! - `GET /repos/{o}/{r}/actions/runs/{run_id}/jobs?filter=latest`: every job of a run that
//!   left the in-flight scan, including jobs too short to be seen running.
//! - `GET /repos/{o}/{r}/actions/jobs/{job_id}`: one job, for jobs seen running that aren't in
//!   the run's latest attempt.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{GithubClient, GithubError, MAX_JOB_PAGES, RawJob, decode};
use crate::jobs::FinishedJob;

#[derive(Deserialize)]
struct RawFinishedJob {
    #[serde(flatten)]
    job: RawJob,
    conclusion: Option<String>,
    completed_at: Option<DateTime<Utc>>,
}

impl RawFinishedJob {
    fn into_finished(self, repo: &str) -> FinishedJob {
        FinishedJob {
            job: self.job.into_model(repo),
            conclusion: self.conclusion,
            completed_at: self.completed_at,
        }
    }
}

/// A run's jobs, and whether the listing was cut short by the page cap.
#[derive(Debug, Clone, PartialEq)]
pub struct RunJobs {
    pub jobs: Vec<FinishedJob>,
    pub truncated: bool,
}

impl GithubClient {
    /// Jobs in the latest attempt of a workflow run, with how each ended. `repo` is `owner/name`.
    pub async fn finished_run_jobs(&self, repo: &str, run_id: u64) -> Result<RunJobs, GithubError> {
        let jobs = self
            .collection::<RawFinishedJob>(
                &format!("/repos/{repo}/actions/runs/{run_id}/jobs?filter=latest"),
                "jobs",
                100,
                MAX_JOB_PAGES,
            )
            .await?;
        Ok(RunJobs {
            jobs: jobs.items.into_iter().map(|j| j.into_finished(repo)).collect(),
            truncated: jobs.truncated,
        })
    }

    /// One job with how it ended. `repo` is `owner/name`.
    pub async fn finished_job(&self, repo: &str, job_id: u64) -> Result<FinishedJob, GithubError> {
        let page = self.get(&format!("/repos/{repo}/actions/jobs/{job_id}")).await?;
        decode::<RawFinishedJob>(page.body).map(|j| j.into_finished(repo))
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::Path;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    use super::*;
    use crate::test_server::serve;

    fn job(id: u64, status: &str, conclusion: Option<&str>) -> serde_json::Value {
        json!({"id": id, "run_id": 11, "name": "build", "workflow_name": "CI", "status": status,
               "conclusion": conclusion, "labels": ["self-hosted", "macOS"], "runner_id": 7, "runner_name": "mini",
               "created_at": "2026-10-03T10:00:00Z", "started_at": "2026-10-03T10:01:00Z",
               "completed_at": conclusion.map(|_| "2026-10-03T10:05:30Z")})
    }

    #[tokio::test]
    async fn reads_run_jobs_and_single_jobs_with_conclusions() {
        let router = Router::new()
            .route(
                "/repos/{o}/{r}/actions/runs/{run}/jobs",
                get(|| async {
                    Json(json!({"total_count": 2, "jobs": [job(111, "completed", Some("success")), job(112, "in_progress", None)]}))
                }),
            )
            .route(
                "/repos/{o}/{r}/actions/jobs/{id}",
                get(|Path((_o, _r, id)): Path<(String, String, u64)>| async move {
                    Json(job(id, "completed", Some("timed_out")))
                }),
            );
        let base = serve(router).await;
        let client = GithubClient::new(&base, "t");
        let run = client.finished_run_jobs("acme/web", 11).await.unwrap();
        assert!(!run.truncated);
        assert_eq!(run.jobs.len(), 2);
        assert_eq!(run.jobs[0].job.repo, "acme/web");
        assert_eq!(run.jobs[0].conclusion.as_deref(), Some("success"));
        assert_eq!(run.jobs[0].completed_at, "2026-10-03T10:05:30Z".parse().ok());
        assert_eq!(run.jobs[1].completed_at, None);
        let single = client.finished_job("acme/web", 999).await.unwrap();
        assert_eq!(single.job.id, 999);
        assert_eq!(single.conclusion.as_deref(), Some("timed_out"));
        assert_eq!(single.job.runner_id, Some(7));
    }

    #[tokio::test]
    async fn a_deleted_job_is_not_found() {
        let base = serve(Router::new()).await;
        let error = GithubClient::new(&base, "t")
            .finished_job("acme/web", 1)
            .await
            .unwrap_err();
        assert!(matches!(error, GithubError::NotFound(_)), "{error:?}");
    }
}
