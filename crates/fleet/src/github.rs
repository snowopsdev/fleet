//! Read-only GitHub REST client for runners, in-flight jobs and the latest runner release.
//!
//! - Org runners: `GET /orgs/{org}/actions/runners` (fine-grained PAT: org
//!   "Self-hosted runners: read"; classic: `admin:org`).
//! - Repository runners: `GET /repos/{owner}/{repo}/actions/runners` (repo
//!   "Administration: read"; classic: `repo`), for personal accounts and repo-scoped runners.
//! - Jobs: there is no org-wide jobs endpoint, so we list
//!   `GET /repos/{o}/{r}/actions/runs?status=in_progress|queued|waiting` per repo in scope,
//!   then `GET .../runs/{id}/jobs` (repo "Actions: read").
//! - Runner release: `GET /repos/actions/runner/releases/latest`.
//! - Every GET is conditional (ETag); a 304 doesn't count against the rate limit.
//! - A 403 or 429 that GitHub marks as a primary or secondary rate limit becomes
//!   [`GithubError::RateLimited`] with the time polling may resume.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, ETAG, HeaderMap, IF_NONE_MATCH, LINK, RETRY_AFTER, USER_AGENT};
use serde::Deserialize;
use serde_json::Value;

use crate::model::{AccountKind, GhJob, GhRunner, JobScan, OrgConfig, RateLimit, RepoScope, RunnerRelease};
use crate::sync::{MutexExt, RwLockExt};

mod completed;

pub const API_VERSION: &str = "2026-03-10";
/// Hard cap on repos scanned per org per cycle, whatever the scope says.
pub const MAX_REPOS: usize = 50;
const CONCURRENCY: usize = 4;
// Bound each collection so a busy repository cannot monopolize a poll cycle.
const MAX_RUN_PAGES: usize = 4;
const MAX_JOB_PAGES: usize = 10;
const MAX_RUNNER_PAGES: usize = 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Conditional responses kept per client. Job pages are keyed by run, so old runs must age out.
const MAX_CACHED_PAGES: usize = 512;
/// GitHub's advice for a secondary rate limit without a `retry-after` header: wait at least a minute.
const SECONDARY_RATE_LIMIT_WAIT: chrono::Duration = chrono::Duration::seconds(60);
/// Workflow-run statuses whose jobs can still need a runner. `waiting` runs hold jobs for an
/// approval; `pending` and `requested` runs wait for a concurrency group or haven't started.
const ACTIVE_RUN_STATUSES: [&str; 5] = ["in_progress", "queued", "waiting", "pending", "requested"];
/// Room a `Recent` scope keeps for extra repositories (runner repositories, recently active
/// ones, a probe), so the most recently pushed repositories can't crowd them out.
pub const MAX_EXTRA_REPOS: usize = 20;
/// Releases read to find when a runner first missed an update.
const RELEASE_PAGE: usize = 30;

#[derive(Debug, Clone)]
struct ApiPage {
    body: Value,
    has_next: bool,
}

/// Recently fetched pages by URL, for conditional requests; the least recently used page goes first.
#[derive(Debug, Default)]
struct EtagCache {
    /// URL -> (use stamp, ETag, page)
    pages: HashMap<String, (u64, String, ApiPage)>,
    /// Use stamp -> URL, oldest first.
    order: BTreeMap<u64, String>,
    next_stamp: u64,
}

impl EtagCache {
    fn get(&mut self, url: &str) -> Option<(String, ApiPage)> {
        let stamp = self.stamp();
        let (used, etag, page) = self.pages.get_mut(url)?;
        let url = self.order.remove(&*used).unwrap_or_else(|| url.to_string());
        *used = stamp;
        let found = (etag.clone(), page.clone());
        self.order.insert(stamp, url);
        Some(found)
    }

    fn insert(&mut self, url: String, etag: String, page: ApiPage) {
        let stamp = self.stamp();
        if let Some((used, ..)) = self.pages.insert(url.clone(), (stamp, etag, page)) {
            self.order.remove(&used);
        }
        self.order.insert(stamp, url);
        while self.order.len() > MAX_CACHED_PAGES {
            if let Some((_, oldest)) = self.order.pop_first() {
                self.pages.remove(&oldest);
            }
        }
    }

    fn stamp(&mut self) -> u64 {
        self.next_stamp += 1;
        self.next_stamp
    }
}

struct Collection<T> {
    items: Vec<T>,
    truncated: bool,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum GithubError {
    #[error("GitHub rejected the token (401). It may be expired or revoked. Replace it in Settings ▸ GitHub.")]
    Unauthorized,
    #[error("GitHub refused access (403): {0}")]
    Forbidden(String),
    #[error("Not found (404): {0}")]
    NotFound(String),
    #[error("GitHub rate limit reached; checks resume at {}.", .resets_at.format("%H:%M UTC"))]
    RateLimited { resets_at: DateTime<Utc> },
    /// A repository's runner list couldn't be read; the text names the repository and the permission.
    #[error("{0}")]
    RepoRunners(String),
    #[error("GitHub returned {0}: {1}")]
    Http(u16, String),
    #[error("Network error: {0}")]
    Network(String),
    #[error("Unexpected response: {0}")]
    Decode(String),
    #[error("GitHub runner listing is incomplete: {0}")]
    Incomplete(String),
}

/// An account's runner listings. Each runner repository is read separately, so one that
/// can't be listed (renamed, or no admin access) doesn't hide the others.
#[derive(Debug, Default)]
pub struct AccountRunners {
    /// Organization runners; empty for a personal account.
    pub org: Vec<GhRunner>,
    /// Each runner repository (name without the owner) with its runners, or why they couldn't be listed.
    pub repos: Vec<(String, Result<Vec<GhRunner>, GithubError>)>,
}

/// One page of an account's repository names, and whether GitHub has more.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoPage {
    pub repos: Vec<String>,
    pub more: bool,
}

/// A GitHub API client for one account. Its token can be replaced without losing the
/// conditional-request cache, so a credential refresh doesn't spend rate limit.
pub struct GithubClient {
    http: reqwest::Client,
    api_base: String,
    token: RwLock<String>,
    cache: Mutex<EtagCache>,
    rate: Mutex<Option<RateLimit>>,
}

/// The token is never shown.
impl std::fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubClient")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl GithubClient {
    pub fn new(api_base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("http client"),
            api_base: api_base.trim_end_matches('/').to_string(),
            token: RwLock::new(token.to_string()),
            cache: Mutex::new(EtagCache::default()),
            rate: Mutex::new(None),
        }
    }

    pub fn rate_limit(&self) -> Option<RateLimit> {
        self.rate.locked().clone()
    }

    /// Use a newly read token for later requests, keeping cached responses.
    pub fn set_token(&self, token: &str) {
        token.clone_into(&mut self.token.write_locked());
    }

    async fn get(&self, path_and_query: &str) -> Result<ApiPage, GithubError> {
        let url = format!("{}{}", self.api_base, path_and_query);
        let cached = self.cache.locked().get(&url);
        let token = self.token.read_locked().clone();
        let mut req = self
            .http
            .get(&url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, "runner-dashboard")
            .bearer_auth(&token);
        if let Some((etag, _)) = &cached {
            req = req.header(IF_NONE_MATCH, etag);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| GithubError::Network(e.without_url().to_string()))?;
        self.record_rate(resp.headers());
        let status = resp.status();

        if status == StatusCode::NOT_MODIFIED
            && let Some((_, body)) = cached
        {
            return Ok(body);
        }
        if !status.is_success() {
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or(body);
            if let Some(resets_at) = rate_limit_reset(status.as_u16(), &headers, &msg, Utc::now()) {
                return Err(GithubError::RateLimited { resets_at });
            }
            return Err(match status.as_u16() {
                401 => GithubError::Unauthorized,
                403 => GithubError::Forbidden(msg),
                404 => GithubError::NotFound(msg),
                code => GithubError::Http(code, msg),
            });
        }
        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let has_next = resp.headers().get_all(LINK).iter().any(|header| {
            header.to_str().is_ok_and(|value| {
                value.split(',').any(|link| {
                    link.split(';').skip(1).any(|parameter| {
                        parameter
                            .trim()
                            .strip_prefix("rel=")
                            .is_some_and(|rel| rel.trim_matches('"').split_whitespace().any(|name| name == "next"))
                    })
                })
            })
        });
        let body: Value = resp.json().await.map_err(|e| GithubError::Decode(e.to_string()))?;
        let body = ApiPage { body, has_next };
        if let Some(etag) = etag {
            self.cache.locked().insert(url, etag, body.clone());
        }
        Ok(body)
    }

    fn record_rate(&self, headers: &HeaderMap) {
        let num = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
        };
        let count = |name: &str| num(name).and_then(|v| u32::try_from(v).ok());
        if let (Some(limit), Some(remaining)) = (count("x-ratelimit-limit"), count("x-ratelimit-remaining")) {
            *self.rate.locked() = Some(RateLimit {
                limit,
                remaining,
                resets_at: num("x-ratelimit-reset").and_then(|t| DateTime::from_timestamp(t, 0)),
            });
        }
    }

    async fn collection<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        field: &str,
        per_page: usize,
        max_pages: usize,
    ) -> Result<Collection<T>, GithubError> {
        let mut items = Vec::new();
        let separator = if path.contains('?') { '&' } else { '?' };
        for page in 1..=max_pages {
            let response = self
                .get(&format!("{path}{separator}per_page={per_page}&page={page}"))
                .await?;
            let total = response.body["total_count"].as_u64();
            let batch: Vec<T> = decode(response.body[field].clone())?;
            let count = batch.len();
            items.extend(batch);
            let more = response.has_next || total.map_or(count == per_page, |total| (items.len() as u64) < total);
            if !more {
                return Ok(Collection {
                    items,
                    truncated: false,
                });
            }
            // An empty page with an advertised continuation is incomplete too.
            if count == 0 {
                return Ok(Collection { items, truncated: true });
            }
        }
        Ok(Collection { items, truncated: true })
    }

    pub async fn org_runners(&self, org: &str) -> Result<Vec<GhRunner>, GithubError> {
        let runners = self
            .collection::<RawRunner>(
                &format!("/orgs/{org}/actions/runners"),
                "runners",
                100,
                MAX_RUNNER_PAGES,
            )
            .await
            .map_err(|e| hint_runner_permission(e, org))?;
        if runners.truncated {
            return Err(GithubError::Incomplete(format!(
                "Couldn't read all of {org}'s runners within {MAX_RUNNER_PAGES} pages."
            )));
        }
        Ok(runners.items.into_iter().map(RawRunner::into_model).collect())
    }

    /// Self-hosted runners registered to one repository, each tagged with `owner/repo`.
    pub async fn repo_runners(&self, owner: &str, repo: &str) -> Result<Vec<GhRunner>, GithubError> {
        let full = format!("{owner}/{repo}");
        let runners = self
            .collection::<RawRunner>(
                &format!("/repos/{full}/actions/runners"),
                "runners",
                100,
                MAX_RUNNER_PAGES,
            )
            .await
            .map_err(|e| hint_repo_runner_permission(e, &full))?;
        if runners.truncated {
            return Err(GithubError::Incomplete(format!(
                "Couldn't read all of {full}'s runners within {MAX_RUNNER_PAGES} pages."
            )));
        }
        Ok(runners
            .items
            .into_iter()
            .map(|raw| GhRunner {
                repo: Some(full.clone()),
                ..raw.into_model()
            })
            .collect())
    }

    /// Every runner listing an account shows: an organization's own runners, and the repository
    /// runners of its `runner_repos`. A failure listing the organization's runners fails the
    /// read; so do a rejected token and a rate limit anywhere. Any other repository failure
    /// is kept beside that repository.
    pub async fn account_runners(&self, account: &OrgConfig) -> Result<AccountRunners, GithubError> {
        let org = match account.kind {
            AccountKind::Organization => self.org_runners(&account.login).await?,
            AccountKind::User => Vec::new(),
        };
        let repos: Vec<(String, Result<Vec<GhRunner>, GithubError>)> =
            stream::iter(account.runner_repos.iter().take(MAX_REPOS).cloned())
                .map(|repo| async move {
                    let listed = self.repo_runners(&account.login, &repo).await;
                    (repo, listed)
                })
                .buffered(CONCURRENCY)
                .collect()
                .await;
        for (_, listed) in &repos {
            if let Err(e @ (GithubError::Unauthorized | GithubError::RateLimited { .. })) = listed {
                return Err(e.clone());
            }
        }
        Ok(AccountRunners { org, repos })
    }

    /// One page of an account's unarchived repository names, sorted by `sort`
    /// (`pushed`, `full_name`, ...). A personal account's private repositories are read
    /// through the signed-in user's list, keeping only those the account owns.
    pub async fn account_repos(
        &self,
        owner: &str,
        kind: AccountKind,
        sort: &str,
        page: u32,
        per_page: usize,
    ) -> Result<RepoPage, GithubError> {
        #[derive(Deserialize)]
        struct Owner {
            login: String,
        }
        #[derive(Deserialize)]
        struct Repo {
            name: String,
            #[serde(default)]
            archived: bool,
            owner: Option<Owner>,
        }
        let direction = if sort == "full_name" { "asc" } else { "desc" };
        let per_page = per_page.clamp(1, 100);
        let path = match kind {
            AccountKind::Organization => format!("/orgs/{owner}/repos?sort={sort}&direction={direction}"),
            AccountKind::User => {
                format!("/user/repos?affiliation=owner,collaborator&sort={sort}&direction={direction}")
            }
        };
        let v = self.get(&format!("{path}&per_page={per_page}&page={page}")).await?;
        let more = v.has_next;
        let repos: Vec<Repo> = decode(v.body)?;
        Ok(RepoPage {
            repos: repos
                .into_iter()
                .filter(|r| !r.archived)
                .filter(|r| r.owner.as_ref().is_none_or(|o| o.login.eq_ignore_ascii_case(owner)))
                .map(|r| r.name)
                .collect(),
            more,
        })
    }

    async fn recent_repos(&self, owner: &str, kind: AccountKind, max: usize) -> Result<Vec<String>, GithubError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        // A personal account's list includes other owners' repositories, filtered out afterwards.
        let per_page = if kind == AccountKind::User { 100 } else { max };
        let mut repos = self.account_repos(owner, kind, "pushed", 1, per_page).await?.repos;
        repos.truncate(max);
        Ok(repos)
    }

    /// Recent published `actions/runner` releases (no drafts or pre-releases), newest first.
    pub async fn runner_releases(&self) -> Result<Vec<RunnerRelease>, GithubError> {
        #[derive(Deserialize)]
        struct Release {
            tag_name: String,
            published_at: Option<DateTime<Utc>>,
            #[serde(default)]
            draft: bool,
            #[serde(default)]
            prerelease: bool,
        }
        let page = self
            .get(&format!("/repos/actions/runner/releases?per_page={RELEASE_PAGE}"))
            .await?;
        let releases: Vec<Release> = decode(page.body)?;
        Ok(releases
            .into_iter()
            .filter(|r| !r.draft && !r.prerelease)
            .map(|r| RunnerRelease {
                version: r.tag_name.trim_start_matches('v').to_string(),
                published_at: r.published_at,
            })
            .collect())
    }

    async fn runs(&self, org: &str, repo: &str, status: &str) -> Result<Collection<u64>, GithubError> {
        #[derive(Deserialize)]
        struct Run {
            id: u64,
        }
        let runs = self
            .collection::<Run>(
                &format!("/repos/{org}/{repo}/actions/runs?status={status}"),
                "workflow_runs",
                50,
                MAX_RUN_PAGES,
            )
            .await?;
        Ok(Collection {
            items: runs.items.into_iter().map(|r| r.id).collect(),
            truncated: runs.truncated,
        })
    }

    async fn run_jobs(&self, org: &str, repo: &str, run_id: u64) -> Result<Collection<GhJob>, GithubError> {
        let jobs = self
            .collection::<RawJob>(
                &format!("/repos/{org}/{repo}/actions/runs/{run_id}/jobs?filter=latest"),
                "jobs",
                100,
                MAX_JOB_PAGES,
            )
            .await?;
        let full = format!("{org}/{repo}");
        Ok(Collection {
            items: jobs.items.into_iter().map(|j| j.into_model(&full)).collect(),
            truncated: jobs.truncated,
        })
    }

    /// Unfinished jobs across an organization's repos in scope.
    pub async fn scan_jobs(&self, org: &str, scope: &RepoScope) -> Result<JobScan, GithubError> {
        self.scan(org, AccountKind::Organization, scope, &[]).await
    }

    /// Unfinished jobs across an account's repos in scope. A `Recent` scope also scans
    /// `extra_repos`, in order, within [`MAX_REPOS`]: up to [`MAX_EXTRA_REPOS`] of them get
    /// room by scanning fewer recently pushed repositories. The scan is marked truncated when
    /// recent repositories or extras give way. Other scopes are exact.
    pub async fn scan_account_jobs(&self, account: &OrgConfig, extra_repos: &[String]) -> Result<JobScan, GithubError> {
        self.scan(&account.login, account.kind, &account.repo_scope, extra_repos)
            .await
    }

    /// Jobs that aren't finished: `queued` and `in_progress`, and the `waiting`, `pending`
    /// and `requested` jobs GitHub holds before queueing them for a runner.
    async fn scan(
        &self,
        org: &str,
        kind: AccountKind,
        scope: &RepoScope,
        extra_repos: &[String],
    ) -> Result<JobScan, GithubError> {
        let mut extras: Vec<&String> = Vec::new();
        if matches!(scope, RepoScope::Recent { max_repos } if *max_repos > 0) {
            for repo in extra_repos {
                if !extras.iter().any(|r| r.eq_ignore_ascii_case(repo)) {
                    extras.push(repo);
                }
            }
        }
        let (mut repos, mut truncated) = match scope {
            RepoScope::None => return Ok(JobScan::default()),
            RepoScope::Selected { repos } => (
                repos.iter().take(MAX_REPOS).cloned().collect::<Vec<_>>(),
                repos.len() > MAX_REPOS,
            ),
            RepoScope::Recent { max_repos } => {
                let wanted = *max_repos as usize;
                let max = wanted.min(MAX_REPOS - extras.len().min(MAX_EXTRA_REPOS));
                (self.recent_repos(org, kind, max).await?, wanted > max)
            }
        };
        for repo in extras {
            if repos.iter().any(|r| r.eq_ignore_ascii_case(repo)) {
                continue;
            }
            if repos.len() >= MAX_REPOS {
                truncated = true;
                break;
            }
            repos.push(repo.clone());
        }

        let results: Vec<Result<Collection<GhJob>, GithubError>> = stream::iter(repos.iter().cloned())
            .map(|repo| async move {
                let mut truncated = false;
                let mut run_ids = Vec::new();
                for status in ACTIVE_RUN_STATUSES {
                    let runs = self.runs(org, &repo, status).await?;
                    truncated |= runs.truncated;
                    run_ids.extend(runs.items);
                }
                run_ids.sort_unstable();
                run_ids.dedup();
                let mut jobs = Vec::new();
                for id in run_ids {
                    let batch = self.run_jobs(org, &repo, id).await?;
                    truncated |= batch.truncated;
                    jobs.extend(batch.items.into_iter().filter(|j| j.status != "completed"));
                }
                Ok(Collection { items: jobs, truncated })
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;

        let mut jobs = Vec::new();
        let mut first_err = None;
        for r in results {
            match r {
                Ok(j) => {
                    truncated |= j.truncated;
                    jobs.extend(j.items);
                }
                // A repo we can't read (e.g. token not granted to it) shouldn't
                // hide the others; mark the scan incomplete instead.
                Err(GithubError::NotFound(_)) | Err(GithubError::Forbidden(_)) => truncated = true,
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        jobs.sort_by_key(|j| j.id);
        Ok(JobScan {
            repos_scanned: repos,
            truncated,
            jobs,
        })
    }
}

/// When a failed response is a rate limit, the time GitHub allows requests again.
///
/// Primary limits send `x-ratelimit-remaining: 0` and a reset time. Secondary limits send
/// 403 or 429 with `retry-after` seconds, or only a message; then GitHub asks for at least a
/// minute's wait. Any other 403 is a permission problem.
fn rate_limit_reset(status: u16, headers: &HeaderMap, message: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if status != 403 && status != 429 {
        return None;
    }
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    if let Some(seconds) = header(RETRY_AFTER.as_str()).and_then(|v| v.parse::<i64>().ok()) {
        return Some(now + chrono::Duration::seconds(seconds.max(1)));
    }
    if header("x-ratelimit-remaining") == Some("0") {
        let reset = header("x-ratelimit-reset")
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|t| DateTime::from_timestamp(t, 0));
        return Some(reset.filter(|t| *t > now).unwrap_or(now + SECONDARY_RATE_LIMIT_WAIT));
    }
    (status == 429 || message.to_ascii_lowercase().contains("rate limit")).then_some(now + SECONDARY_RATE_LIMIT_WAIT)
}

fn hint_runner_permission(e: GithubError, org: &str) -> GithubError {
    match e {
        GithubError::Forbidden(msg) | GithubError::NotFound(msg) => GithubError::Forbidden(format!(
            "{msg}. Listing {org}'s runners needs a fine-grained token with the organization permission \"Self-hosted runners: read\" (classic tokens: admin:org), from an org owner."
        )),
        other => other,
    }
}

fn hint_repo_runner_permission(e: GithubError, repo: &str) -> GithubError {
    match e {
        GithubError::Forbidden(msg) | GithubError::NotFound(msg) => GithubError::RepoRunners(format!(
            "Couldn't list {repo}'s runners: {msg}. That needs admin access to the repository, with the fine-grained repository permission \"Administration: read\" (classic tokens and gh: the repo scope). If it was renamed or deleted, update the account's runner repositories in Settings ▸ GitHub."
        )),
        other => other,
    }
}

fn decode<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, GithubError> {
    serde_json::from_value(v).map_err(|e| GithubError::Decode(e.to_string()))
}

#[derive(Deserialize)]
struct RawRunner {
    id: u64,
    name: String,
    #[serde(default)]
    os: String,
    status: String,
    busy: bool,
    #[serde(default)]
    labels: Vec<RawLabel>,
    ephemeral: Option<bool>,
    version: Option<String>,
    runner_group_id: Option<u64>,
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
}

impl RawRunner {
    fn into_model(self) -> GhRunner {
        GhRunner {
            id: self.id,
            name: self.name,
            os: self.os,
            status: self.status,
            busy: self.busy,
            labels: self.labels.into_iter().map(|l| l.name).collect(),
            ephemeral: self.ephemeral,
            version: self.version,
            runner_group_id: self.runner_group_id,
            repo: None,
        }
    }
}

#[derive(Deserialize)]
struct RawJob {
    id: u64,
    run_id: u64,
    name: String,
    workflow_name: Option<String>,
    status: String,
    #[serde(default)]
    labels: Vec<String>,
    runner_id: Option<u64>,
    runner_name: Option<String>,
    created_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    html_url: Option<String>,
    head_branch: Option<String>,
}

impl RawJob {
    fn into_model(self, repo: &str) -> GhJob {
        GhJob {
            id: self.id,
            run_id: self.run_id,
            repo: repo.to_string(),
            name: self.name,
            workflow_name: self.workflow_name,
            status: self.status,
            labels: self.labels,
            runner_id: self.runner_id.filter(|id| *id != 0),
            runner_name: self.runner_name.filter(|n| !n.is_empty()),
            created_at: self.created_at,
            started_at: self.started_at,
            html_url: self.html_url,
            head_branch: self.head_branch,
        }
    }
}

#[cfg(test)]
#[path = "github_tests.rs"]
mod tests;
