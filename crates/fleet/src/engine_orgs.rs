//! GitHub accounts: adding and signing in, the cached API clients, and runner, job and release polls.
//!
//! Each account keeps one client between polls, so its conditional-request cache survives
//! token re-reads. Rejected credentials are read again on the next poll, at most every
//! two minutes unless Refresh Now asks. A rate-limited account isn't polled until GitHub's
//! reset time, plus a few seconds.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::future::{BoxFuture, join_all};

use super::{Engine, EngineError, SecretStore};
use crate::derive::GITHUB_STALE_AFTER;
use crate::github::{GithubClient, GithubError, MAX_REPOS};
use crate::model::{AccountKind, JobScan, OrgAuth, OrgConfig, OrgObservation, RepoScope, Timed, default_api_base};
use crate::sync::{MutexExt, RwLockExt};
use crate::{gh, labels};

/// The least time between token re-reads after GitHub rejects one, unless Refresh Now asks.
const TOKEN_REREAD_EVERY: Duration = Duration::from_secs(120);
/// How often the latest `actions/runner` release is read.
const RELEASE_CHECK_EVERY: chrono::Duration = chrono::Duration::hours(1);
/// How long a repository where self-hosted jobs ran stays in a `Recent` job scan.
const REMEMBER_ACTIVE_REPOS: chrono::Duration = chrono::Duration::days(7);
/// Repositories probed per job scan while a busy runner's job hasn't been found.
const PROBE_REPOS: usize = 10;

#[derive(Debug, thiserror::Error)]
enum GithubAuthError {
    #[error("No token in the Keychain for this organization.")]
    MissingToken,
    #[error("Couldn't read the GitHub token from the Keychain: {0}")]
    Secret(String),
    #[error("{0}")]
    Gh(#[from] gh::GhError),
}

/// How an added account is watched; see [`OrgConfig`].
#[derive(Debug, Clone, PartialEq)]
pub struct AccountSetup {
    pub kind: AccountKind,
    pub repo_scope: RepoScope,
    /// Repositories whose repository runners to include; required for personal accounts.
    pub runner_repos: Vec<String>,
}

/// One account's GitHub state between polls. Nothing here is persisted.
#[derive(Debug, Default)]
pub(super) struct AccountState {
    client: Option<Arc<GithubClient>>,
    /// GitHub rejected the token: read it again before the next poll.
    reread_token: bool,
    token_reread_at: Option<Instant>,
    /// Lowercase repository name -> (name, when a self-hosted job was last seen there).
    active_repos: BTreeMap<String, (String, DateTime<Utc>)>,
    /// The next page of repositories, by name, to probe for a busy runner's job.
    probe_page: u32,
}

/// Fetches one kind of GitHub data for an org. Boxed, so the poll future is provably `Send`.
type GithubFetch<T> = for<'a> fn(&'a Engine, &'a GithubClient, &'a OrgConfig) -> BoxFuture<'a, Result<T, GithubError>>;

/// Where one kind of GitHub data, and the latest error fetching it, live in an org's observations.
type ObservationSlot<T> = fn(&mut OrgObservation) -> (&mut Option<Timed<T>>, &mut Option<Timed<String>>);

/// Resumes a rate-limited account a few seconds after GitHub's reset, spread so that
/// several accounts sharing a login don't all retry at once.
fn resume_jitter(now: DateTime<Utc>) -> chrono::Duration {
    chrono::Duration::milliseconds(1000 + i64::from(now.timestamp_subsec_millis() % 4000))
}

/// Repository names for `runner_repos`: an owner prefix must name the account, and duplicates go.
fn runner_repo_names(login: &str, kind: AccountKind, repos: &[String]) -> Result<Vec<String>, EngineError> {
    let mut names: Vec<String> = Vec::new();
    for entry in repos.iter().map(|r| r.trim()).filter(|r| !r.is_empty()) {
        let (owner, name) = entry.rsplit_once('/').map_or((None, entry), |(o, n)| (Some(o), n));
        if let Some(owner) = owner.filter(|o| !o.eq_ignore_ascii_case(login)) {
            return Err(EngineError::Invalid(format!(
                "{entry} belongs to {owner}, not {login}. Add {owner} as its own account to watch its runners."
            )));
        }
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(EngineError::Invalid(format!("`{entry}` isn't a repository name.")));
        }
        if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            names.push(name.to_string());
        }
    }
    if names.len() > MAX_REPOS {
        return Err(EngineError::Invalid(format!(
            "Watch at most {MAX_REPOS} repositories' runners per account."
        )));
    }
    if kind == AccountKind::User && names.is_empty() {
        return Err(EngineError::Invalid(
            "A personal account has no account-wide runner list. Enter the repositories whose runners to show.".into(),
        ));
    }
    Ok(names)
}

impl Engine {
    /// Keychain calls can block, so they run off the async runtime.
    async fn secrets_call<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn SecretStore) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || call(secrets.as_ref()))
            .await
            .map_err(|e| format!("The Keychain request stopped unexpectedly: {e}"))?
    }

    fn find_org(&self, login: &str) -> Result<OrgConfig, EngineError> {
        let key = login.to_lowercase();
        self.config()
            .orgs
            .into_iter()
            .find(|o| o.login.to_lowercase() == key)
            .ok_or_else(|| {
                EngineError::NotFound(format!(
                    "{login} isn't an added organization. Add it in Settings ▸ GitHub first."
                ))
            })
    }

    async fn read_token(&self, org: &OrgConfig) -> Result<String, GithubAuthError> {
        Ok(match org.auth {
            OrgAuth::Token => {
                let account = org.login.to_lowercase();
                self.secrets_call(move |secrets| secrets.get(&account))
                    .await
                    .map_err(GithubAuthError::Secret)?
                    .ok_or(GithubAuthError::MissingToken)?
            }
            OrgAuth::GhCli => gh::token(&gh::host_for_api_base(&org.api_base)).await?,
        })
    }

    /// API client for an org, with its token from the Keychain or from `gh`. A token GitHub
    /// rejected is read again (throttled), and swapped into the same client.
    /// `Err` carries a user-facing reason (no token, gh missing or logged out).
    async fn github_client(&self, org: &OrgConfig) -> Result<Arc<GithubClient>, GithubAuthError> {
        let key = org.login.to_lowercase();
        let existing = {
            let mut accounts = self.github.locked();
            let state = accounts.entry(key.clone()).or_default();
            let reuse = !state.reread_token
                || state
                    .token_reread_at
                    .is_some_and(|at| at.elapsed() < TOKEN_REREAD_EVERY);
            match state.client.clone() {
                Some(client) if reuse => return Ok(client),
                Some(client) => {
                    state.token_reread_at = Some(Instant::now());
                    Some(client)
                }
                None => None,
            }
        };
        let token = self.read_token(org).await?;
        let mut accounts = self.github.locked();
        let state = accounts.entry(key).or_default();
        if let (None, Some(current)) = (&existing, &state.client) {
            // Signed in again while the token was read.
            return Ok(current.clone());
        }
        let client = match existing {
            Some(client) => {
                client.set_token(&token);
                client
            }
            None => Arc::new(GithubClient::new(&org.api_base, &token)),
        };
        state.client = Some(client.clone());
        state.reread_token = false;
        Ok(client)
    }

    /// Use a client whose credential was just checked.
    fn install_client(&self, key: &str, client: GithubClient) {
        let mut accounts = self.github.locked();
        let state = accounts.entry(key.to_string()).or_default();
        state.client = Some(Arc::new(client));
        state.reread_token = false;
    }

    /// GitHub errors, reworded for how this org signs in.
    fn github_error_text(&self, org: &OrgConfig, e: &GithubError) -> String {
        match (org.auth, org.kind, e) {
            (OrgAuth::GhCli, AccountKind::Organization, GithubError::Forbidden(_)) => format!(
                "Your gh login can't list {}'s runners. GitHub requires the {} scope (and org owner rights) for that. Run `{}` in Terminal.",
                org.login,
                gh::RUNNERS_SCOPE,
                gh::refresh_command(&gh::host_for_api_base(&org.api_base))
            ),
            (OrgAuth::GhCli, _, GithubError::Unauthorized) => {
                "gh's token was rejected. Run `gh auth login` in Terminal; the app picks up the new login automatically.".into()
            }
            _ => e.to_string(),
        }
    }

    /// Read the token again before the next poll (e.g. after `gh auth refresh`), keeping the client's cache.
    fn distrust_token(&self, org: &OrgConfig) {
        if let Some(state) = self.github.locked().get_mut(&org.login.to_lowercase()) {
            state.reread_token = true;
        }
    }

    /// The client the regular polls created for an account, unless it's rate-limited. Job
    /// history reads use it so they share the account's token, ETag cache and backoff.
    pub(super) fn polled_client(&self, key: &str, now: DateTime<Utc>) -> Option<Arc<GithubClient>> {
        if self.rate_limited(key, now) {
            return None;
        }
        self.github.locked().get(key).and_then(|state| state.client.clone())
    }

    fn rate_limited(&self, key: &str, now: DateTime<Utc>) -> bool {
        self.obs
            .read_locked()
            .github
            .get(key)
            .and_then(|o| o.rate_limited_until)
            .is_some_and(|until| until > now)
    }

    /// Fetch one kind of GitHub data for every org, keeping the last good value beside the newest error.
    /// Rate-limited orgs are skipped until GitHub's reset time.
    async fn poll_github<T>(
        &self,
        fetch: GithubFetch<T>,
        distrust_token_on: fn(&GithubError) -> bool,
        slot: ObservationSlot<T>,
    ) {
        let orgs = self.config().orgs;
        join_all(orgs.iter().map(|org| async {
            let key = org.login.to_lowercase();
            if self.rate_limited(&key, Utc::now()) {
                return;
            }
            let client = match self.github_client(org).await {
                Ok(client) => client,
                Err(reason) => {
                    let mut obs = self.obs.write_locked();
                    let entry = obs.github.entry(key).or_default();
                    entry.missing_token = matches!(reason, GithubAuthError::MissingToken);
                    *slot(entry).1 = Some(Timed::new(Utc::now(), reason.to_string()));
                    return;
                }
            };
            let result = fetch(self, &client, org).await;
            if result.as_ref().is_err_and(distrust_token_on) {
                self.distrust_token(org);
            }
            let now = Utc::now();
            let mut obs = self.obs.write_locked();
            let entry = obs.github.entry(key).or_default();
            entry.missing_token = false;
            entry.rate_limit = client.rate_limit();
            entry.rate_limited_until = match &result {
                Err(GithubError::RateLimited { resets_at }) => Some((*resets_at).max(now) + resume_jitter(now)),
                _ => None,
            };
            let (value, error) = slot(entry);
            match result {
                Ok(fetched) => {
                    *value = Some(Timed::new(now, fetched));
                    *error = None;
                }
                Err(e) => *error = Some(Timed::new(now, self.github_error_text(org, &e))),
            }
        }))
        .await;
    }

    /// Organization and repository runners for every account, then the runner release when due.
    pub async fn poll_github_runners(&self) {
        self.poll_github(
            |_, client, org| client.account_runners(org).boxed(),
            // A 403 here means the credential can't list runners. Fetching the token
            // again picks up a fix such as `gh auth refresh --scopes admin:org`.
            |e| {
                matches!(
                    e,
                    GithubError::Unauthorized | GithubError::Forbidden(_) | GithubError::RepoRunners(_)
                )
            },
            |org| (&mut org.runners, &mut org.error),
        )
        .await;
        self.poll_runner_release().await;
    }

    pub async fn poll_github_jobs(&self) {
        self.poll_github(
            |engine, client, org| {
                async move {
                    let extra = engine.scan_extras(client, org).await?;
                    let scan = client.scan_account_jobs(org, &extra).await?;
                    engine.remember_active_repos(org, &scan);
                    Ok(scan)
                }
                .boxed()
            },
            // Repositories the token can't read only mark the scan incomplete, so a 403
            // says nothing about the credential.
            |e| matches!(e, GithubError::Unauthorized),
            |org| (&mut org.jobs, &mut org.jobs_error),
        )
        .await;
    }

    /// Repositories a `Recent` job scan adds to the most recently pushed ones: the account's
    /// runner repositories, repositories where self-hosted jobs ran in the last week, and,
    /// while a busy runner's job hasn't been found, the next page of its other repositories.
    /// Scheduled and manually started workflows often run in repositories nobody pushed to.
    async fn scan_extras(&self, client: &GithubClient, org: &OrgConfig) -> Result<Vec<String>, GithubError> {
        if !matches!(org.repo_scope, RepoScope::Recent { max_repos } if max_repos > 0) {
            return Ok(Vec::new());
        }
        let key = org.login.to_lowercase();
        let now = Utc::now();
        let probe = self.busy_runner_without_job(&key, now);
        let mut repos = org.runner_repos.clone();
        let page = {
            let mut accounts = self.github.locked();
            let state = accounts.entry(key.clone()).or_default();
            state
                .active_repos
                .retain(|_, (_, seen)| now - *seen <= REMEMBER_ACTIVE_REPOS);
            repos.extend(state.active_repos.values().map(|(name, _)| name.clone()));
            probe.then_some(state.probe_page.max(1))
        };
        if let Some(page) = page {
            let listing = client
                .account_repos(&org.login, org.kind, "full_name", page, PROBE_REPOS)
                .await?;
            if let Some(state) = self.github.locked().get_mut(&key) {
                state.probe_page = if listing.more { page + 1 } else { 1 };
            }
            repos.extend(listing.repos);
        }
        Ok(repos)
    }

    /// Whether fresh GitHub data shows a busy runner that the last job scan doesn't link to a job.
    fn busy_runner_without_job(&self, key: &str, now: DateTime<Utc>) -> bool {
        let obs = self.obs.read_locked();
        let Some(org) = obs.github.get(key) else {
            return false;
        };
        let Some(runners) = org.runners.as_ref().filter(|r| now - r.at <= GITHUB_STALE_AFTER) else {
            return false;
        };
        let jobs = org.jobs.as_ref().map(|j| j.value.jobs.as_slice()).unwrap_or_default();
        runners.value.iter().any(|runner| {
            runner.busy
                && !jobs
                    .iter()
                    .any(|j| j.status == "in_progress" && j.runner_id == Some(runner.id))
        })
    }

    /// Keep scanning repositories where self-hosted jobs were seen.
    fn remember_active_repos(&self, org: &OrgConfig, scan: &JobScan) {
        let key = org.login.to_lowercase();
        let now = Utc::now();
        let runner_ids: HashSet<u64> = self
            .obs
            .read_locked()
            .github
            .get(&key)
            .and_then(|o| o.runners.as_ref())
            .map(|r| r.value.iter().map(|r| r.id).collect())
            .unwrap_or_default();
        let mut accounts = self.github.locked();
        let state = accounts.entry(key).or_default();
        for job in &scan.jobs {
            if labels::targets_self_hosted(&job.labels) || job.runner_id.is_some_and(|id| runner_ids.contains(&id)) {
                let name = job.repo.rsplit_once('/').map_or(job.repo.as_str(), |(_, name)| name);
                state.active_repos.insert(name.to_lowercase(), (name.to_string(), now));
            }
        }
    }

    /// Read the latest `actions/runner` release at most hourly, through the first github.com
    /// account that isn't rate-limited. GitHub Enterprise Server accounts are never used.
    pub async fn poll_runner_release(&self) {
        let now = Utc::now();
        if self
            .obs
            .read_locked()
            .runner_release_checked_at
            .is_some_and(|at| now - at < RELEASE_CHECK_EVERY)
        {
            return;
        }
        let Some(org) = self.config().orgs.into_iter().find(|o| {
            o.api_base.trim_end_matches('/') == default_api_base() && !self.rate_limited(&o.login.to_lowercase(), now)
        }) else {
            return;
        };
        let Ok(client) = self.github_client(&org).await else {
            return;
        };
        let result = client.latest_runner_release().await;
        let mut obs = self.obs.write_locked();
        obs.runner_release_checked_at = Some(Utc::now());
        match result {
            Ok(release) => obs.runner_release = Some(Timed::new(Utc::now(), release)),
            Err(e) => tracing::warn!("couldn't read the latest runner release: {e}"),
        }
    }

    /// Adds an organization after checking the credential can list its runners.
    /// `GhCli` uses the GitHub CLI's login; `Token` stores `token` in the Keychain.
    pub async fn add_org(
        &self,
        login: &str,
        auth: OrgAuth,
        token: Option<&str>,
        repo_scope: RepoScope,
        api_base: Option<String>,
    ) -> Result<(), EngineError> {
        let setup = AccountSetup {
            kind: AccountKind::Organization,
            repo_scope,
            runner_repos: Vec::new(),
        };
        self.add_account(login, setup, auth, token, api_base).await
    }

    /// Adds an organization or personal account after checking the credential can list
    /// every runner it should show. `GhCli` uses the GitHub CLI's login; `Token` stores
    /// `token` in the Keychain.
    pub async fn add_account(
        &self,
        login: &str,
        setup: AccountSetup,
        auth: OrgAuth,
        token: Option<&str>,
        api_base: Option<String>,
    ) -> Result<(), EngineError> {
        let login = login.trim().trim_start_matches('@').to_string();
        if login.is_empty() || login.contains('/') || login.contains(' ') {
            return Err(EngineError::Invalid(
                "Enter an organization or account login, like `acme`.".into(),
            ));
        }
        let key = login.to_lowercase();
        if self.config().orgs.iter().any(|o| o.login.to_lowercase() == key) {
            return Err(EngineError::Invalid(format!("{login} is already added.")));
        }
        let runner_repos = runner_repo_names(&login, setup.kind, &setup.runner_repos)?;
        let api_base = api_base.unwrap_or_else(default_api_base);
        let org = OrgConfig {
            login: login.clone(),
            api_base: api_base.clone(),
            repo_scope: setup.repo_scope,
            added_at: Utc::now(),
            auth,
            kind: setup.kind,
            runner_repos,
        };
        let token = match auth {
            OrgAuth::GhCli => gh::token(&gh::host_for_api_base(&api_base))
                .await
                .map_err(|e| EngineError::Invalid(e.to_string()))?,
            OrgAuth::Token => {
                let t = token.map(str::trim).unwrap_or_default();
                if t.is_empty() {
                    return Err(EngineError::Invalid("A token is required.".into()));
                }
                t.to_string()
            }
        };
        let client = GithubClient::new(&api_base, &token);
        let runners = client
            .account_runners(&org)
            .await
            .map_err(|e| EngineError::Invalid(self.github_error_text(&org, &e)))?;

        if auth == OrgAuth::Token {
            let account = key.clone();
            self.secrets_call(move |secrets| secrets.set(&account, &token))
                .await
                .map_err(EngineError::Storage)?;
        }
        self.install_client(&key, client);
        self.update_config(|c| c.orgs.push(org))?;
        let mut obs = self.obs.write_locked();
        let entry = obs.github.entry(key).or_default();
        entry.runners = Some(Timed::new(Utc::now(), runners));
        entry.missing_token = false;
        entry.error = None;
        Ok(())
    }

    /// Switches an org to the GitHub CLI login (after checking it works) and
    /// removes any stored token.
    pub async fn use_gh_for_org(&self, login: &str) -> Result<(), EngineError> {
        let key = login.to_lowercase();
        let mut org = self.find_org(login)?;
        org.auth = OrgAuth::GhCli;
        let token = gh::token(&gh::host_for_api_base(&org.api_base))
            .await
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
        let client = GithubClient::new(&org.api_base, &token);
        client
            .account_runners(&org)
            .await
            .map_err(|e| EngineError::Invalid(self.github_error_text(&org, &e)))?;
        self.update_config(|c| {
            if let Some(o) = c.orgs.iter_mut().find(|o| o.login.to_lowercase() == key) {
                o.auth = OrgAuth::GhCli;
            }
        })?;
        self.delete_token(&key).await;
        self.install_client(&key, client);
        Ok(())
    }

    /// Re-reads gh's login on the next poll (after `gh auth refresh` / `login`), keeping cached responses.
    pub fn reload_gh_credentials(&self) {
        let orgs = self.config().orgs;
        let mut accounts = self.github.locked();
        for o in orgs.iter().filter(|o| o.auth == OrgAuth::GhCli) {
            if let Some(state) = accounts.get_mut(&o.login.to_lowercase()) {
                state.reread_token = true;
                state.token_reread_at = None;
            }
        }
    }

    pub async fn update_org_token(&self, login: &str, token: &str) -> Result<(), EngineError> {
        let key = login.to_lowercase();
        let org = self.find_org(login)?;
        let token = token.trim().to_string();
        let client = GithubClient::new(&org.api_base, &token);
        client.account_runners(&org).await?;
        let account = key.clone();
        self.secrets_call(move |secrets| secrets.set(&account, &token))
            .await
            .map_err(EngineError::Storage)?;
        self.update_config(|c| {
            if let Some(o) = c.orgs.iter_mut().find(|o| o.login.to_lowercase() == key) {
                o.auth = OrgAuth::Token;
            }
        })?;
        self.install_client(&key, client);
        Ok(())
    }

    pub fn set_org_scope(&self, login: &str, scope: RepoScope) -> Result<(), EngineError> {
        let key = login.to_lowercase();
        self.update_config(|c| {
            if let Some(o) = c.orgs.iter_mut().find(|o| o.login.to_lowercase() == key) {
                o.repo_scope = scope;
            }
        })
    }

    /// Replaces the repositories whose runners an account shows, after checking they can be listed.
    pub async fn set_org_runner_repos(&self, login: &str, repos: &[String]) -> Result<(), EngineError> {
        let mut org = self.find_org(login)?;
        org.runner_repos = runner_repo_names(&org.login, org.kind, repos)?;
        let client = self
            .github_client(&org)
            .await
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
        let runners = client
            .account_runners(&org)
            .await
            .map_err(|e| EngineError::Invalid(self.github_error_text(&org, &e)))?;
        let key = org.login.to_lowercase();
        self.update_config(|c| {
            if let Some(o) = c.orgs.iter_mut().find(|o| o.login.to_lowercase() == key) {
                o.runner_repos.clone_from(&org.runner_repos);
            }
        })?;
        let mut obs = self.obs.write_locked();
        let entry = obs.github.entry(key).or_default();
        entry.runners = Some(Timed::new(Utc::now(), runners));
        entry.error = None;
        Ok(())
    }

    pub async fn remove_org(&self, login: &str) -> Result<(), EngineError> {
        let key = login.to_lowercase();
        self.update_config(|c| c.orgs.retain(|o| o.login.to_lowercase() != key))?;
        self.delete_token(&key).await;
        self.github.locked().remove(&key);
        self.obs.write_locked().github.remove(&key);
        Ok(())
    }

    /// The org no longer uses a stored token, so a failed delete only leaves an unused Keychain item.
    async fn delete_token(&self, key: &str) {
        let account = key.to_string();
        if let Err(e) = self.secrets_call(move |secrets| secrets.delete(&account)).await {
            tracing::warn!("couldn't delete the stored GitHub token for {key}: {e}");
        }
    }
}
