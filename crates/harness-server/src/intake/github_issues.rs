use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use harness_core::config::isolation::IsolationTrustClass;
use reqwest::header::{HeaderMap, ACCEPT, LINK, RETRY_AFTER, USER_AGENT};
use reqwest::StatusCode;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{IncomingIssue, IntakeSource, TaskCompletionResult};
use crate::task_runner::TaskId;

const GITHUB_ISSUES_MAX_PAGES: usize = 20;
const DEFAULT_RATE_LIMIT_RETRY_SECS: i64 = 60;

#[async_trait]
pub(crate) trait DispatchedTaskChecker: Send + Sync {
    async fn exists(&self, task_id: &TaskId) -> anyhow::Result<bool>;
}

#[async_trait]
impl DispatchedTaskChecker for crate::task_runner::TaskStore {
    async fn exists(&self, task_id: &TaskId) -> anyhow::Result<bool> {
        self.exists_with_db_fallback(task_id).await
    }
}

#[cfg(test)]
pub(crate) struct RuntimeAwareDispatchedTaskChecker {
    tasks: Arc<crate::task_runner::TaskStore>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
}

#[cfg(test)]
impl RuntimeAwareDispatchedTaskChecker {
    pub(crate) fn new(
        tasks: Arc<crate::task_runner::TaskStore>,
        workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    ) -> Self {
        Self {
            tasks,
            workflow_runtime_store,
        }
    }
}

#[cfg(test)]
#[async_trait]
impl DispatchedTaskChecker for RuntimeAwareDispatchedTaskChecker {
    async fn exists(&self, task_id: &TaskId) -> anyhow::Result<bool> {
        if self.tasks.exists_with_db_fallback(task_id).await? {
            return Ok(true);
        }
        let Some(store) = self.workflow_runtime_store.as_ref() else {
            return Ok(false);
        };
        Ok(store
            .get_instance_by_task_id(task_id.as_str())
            .await?
            .is_some())
    }
}

pub struct GitHubIssuesPoller {
    repo: String,
    label: String,
    client: reqwest::Client,
    project_root: Option<PathBuf>,
    dispatched: DashMap<String, TaskId>,
    persist_path: Option<PathBuf>,
    task_checker: Option<Arc<dyn DispatchedTaskChecker>>,
    github_token: Option<String>,
    throttle: Arc<GitHubRateLimitThrottle>,
}

#[derive(Debug, Default)]
pub struct GitHubRateLimitThrottle {
    retry_after: Mutex<Option<DateTime<Utc>>>,
}

impl GitHubRateLimitThrottle {
    fn current_retry_after(&self, now: DateTime<Utc>) -> anyhow::Result<Option<DateTime<Utc>>> {
        let mut retry_after = self
            .retry_after
            .lock()
            .map_err(|_| anyhow::anyhow!("GitHub rate-limit throttle lock poisoned"))?;
        if retry_after.as_ref().is_some_and(|at| *at <= now) {
            *retry_after = None;
        }
        Ok(retry_after.to_owned())
    }

    fn record_retry_after(&self, retry_at: DateTime<Utc>) -> anyhow::Result<()> {
        let mut retry_after = self
            .retry_after
            .lock()
            .map_err(|_| anyhow::anyhow!("GitHub rate-limit throttle lock poisoned"))?;
        if retry_after
            .as_ref()
            .is_none_or(|current| retry_at > *current)
        {
            *retry_after = Some(retry_at);
        }
        Ok(())
    }
}

impl GitHubIssuesPoller {
    pub fn new(
        repo_config: &harness_core::config::intake::GitHubRepoConfig,
        data_dir: Option<&Path>,
    ) -> Self {
        Self::new_with_token(repo_config, data_dir, None)
    }

    pub fn new_with_token(
        repo_config: &harness_core::config::intake::GitHubRepoConfig,
        data_dir: Option<&Path>,
        github_token: Option<String>,
    ) -> Self {
        Self::new_with_token_and_throttle(
            repo_config,
            data_dir,
            github_token,
            Arc::new(GitHubRateLimitThrottle::default()),
        )
    }

    pub fn new_with_token_and_throttle(
        repo_config: &harness_core::config::intake::GitHubRepoConfig,
        data_dir: Option<&Path>,
        github_token: Option<String>,
        throttle: Arc<GitHubRateLimitThrottle>,
    ) -> Self {
        let repo_slug = repo_config.repo.replace('/', "_");
        let persist_path = data_dir.map(|d| d.join(format!("github_dispatched_{repo_slug}.json")));
        let dispatched = Self::load_dispatched(persist_path.as_deref());
        Self {
            repo: repo_config.repo.clone(),
            label: repo_config.label.clone(),
            client: reqwest::Client::new(),
            project_root: repo_config.project_root.as_ref().map(PathBuf::from),
            dispatched,
            persist_path,
            task_checker: None,
            github_token,
            throttle,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_task_checker(
        mut self,
        task_checker: Arc<dyn DispatchedTaskChecker>,
    ) -> Self {
        self.task_checker = Some(task_checker);
        self
    }

    fn load_dispatched(path: Option<&Path>) -> DashMap<String, TaskId> {
        let Some(path) = path else {
            return DashMap::new();
        };
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DashMap::new(),
            Err(e) => {
                tracing::warn!(
                    "failed to read dispatched state from {}: {e}",
                    path.display()
                );
                return DashMap::new();
            }
        };
        let map: HashMap<String, String> = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    "failed to load dispatched state from {}: {e}",
                    path.display()
                );
                return DashMap::new();
            }
        };
        let dm = DashMap::new();
        for (k, v) in map {
            dm.insert(
                normalize_issue_external_id(&k),
                harness_core::types::TaskId(v),
            );
        }
        dm
    }

    fn persist_dispatched(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("failed to create dispatched state directory: {e}");
                return;
            }
        }
        let map: HashMap<String, String> = self
            .dispatched
            .iter()
            .map(|e| (e.key().clone(), e.value().0.clone()))
            .collect();
        match serde_json::to_vec(&map) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    tracing::warn!("failed to persist dispatched state: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to serialize dispatched state: {e}"),
        }
    }

    fn is_synthetic_skip_marker(task_id: &TaskId) -> bool {
        task_id.0.starts_with("skip-")
    }

    async fn prune_missing_task_entries(&self) -> anyhow::Result<usize> {
        let Some(task_checker) = &self.task_checker else {
            return Ok(0);
        };

        let dispatched: Vec<(String, TaskId)> = self
            .dispatched
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        let mut stale_issue_ids = Vec::new();

        for (issue_id, task_id) in dispatched {
            if Self::is_synthetic_skip_marker(&task_id) {
                continue;
            }
            match task_checker.exists(&task_id).await {
                Ok(true) => {}
                Ok(false) => stale_issue_ids.push(issue_id),
                Err(e) => {
                    tracing::warn!(
                        repo = %self.repo,
                        issue_id,
                        task_id = %task_id.0,
                        "intake: failed to verify dispatched task existence: {e}"
                    );
                }
            }
        }

        if !stale_issue_ids.is_empty() {
            for issue_id in &stale_issue_ids {
                self.dispatched.remove(issue_id);
            }
            self.persist_dispatched();
        }

        Ok(stale_issue_ids.len())
    }

    pub async fn reconcile_dispatched_with_store(&self) -> anyhow::Result<usize> {
        self.prune_missing_task_entries().await
    }

    async fn poll_from_api_base_url(
        &self,
        api_base_url: &str,
    ) -> anyhow::Result<Vec<IncomingIssue>> {
        let now = Utc::now();
        if let Some(retry_at) = self.throttle.current_retry_after(now)? {
            tracing::debug!(
                repo = %self.repo,
                retry_at = %retry_at.to_rfc3339(),
                "GitHub issue polling skipped while token is rate-limited"
            );
            return Ok(Vec::new());
        }

        let mut next_url = Some(github_issues_url(api_base_url, &self.repo, &self.label)?);
        let mut seen_urls = HashSet::new();
        let mut page_count = 0usize;
        let mut complete_open_issue_set = true;
        let mut new_issues = Vec::new();
        let mut open_issue_ids = HashSet::new();

        while let Some(url) = next_url {
            if page_count >= GITHUB_ISSUES_MAX_PAGES {
                tracing::warn!(
                    repo = %self.repo,
                    page_limit = GITHUB_ISSUES_MAX_PAGES,
                    "GitHub issue polling stopped after reaching the pagination page limit"
                );
                complete_open_issue_set = false;
                break;
            }
            page_count += 1;

            if !seen_urls.insert(url.clone()) {
                tracing::warn!(
                    repo = %self.repo,
                    url = %url,
                    "GitHub issue polling stopped because pagination repeated a URL"
                );
                complete_open_issue_set = false;
                break;
            }

            let page = match fetch_github_issue_page(
                &self.client,
                &url,
                &self.repo,
                self.github_token.as_deref(),
            )
            .await?
            {
                GitHubIssuePageFetch::Page(page) => page,
                GitHubIssuePageFetch::RateLimited(limit) => {
                    self.throttle.record_retry_after(limit.retry_at)?;
                    tracing::warn!(
                        repo = %self.repo,
                        status = %limit.status,
                        retry_at = %limit.retry_at.to_rfc3339(),
                        "GitHub issue polling paused after rate-limit response"
                    );
                    complete_open_issue_set = false;
                    break;
                }
            };
            let parsed = parse_gh_output(
                &page.body,
                &self.repo,
                &self.dispatched,
                self.project_root.as_deref(),
            )?;
            new_issues.extend(parsed.new_issues);
            open_issue_ids.extend(parsed.open_issue_ids);
            next_url = page.next_url;
        }

        if complete_open_issue_set {
            self.evict_closed_dispatched_entries(&open_issue_ids);
        } else {
            tracing::debug!(
                repo = %self.repo,
                "intake: skipped dispatched eviction because GitHub issue pagination was incomplete"
            );
        }

        Ok(new_issues)
    }

    fn evict_closed_dispatched_entries(&self, open_issue_ids: &HashSet<String>) {
        let stale: Vec<String> = self
            .dispatched
            .iter()
            .map(|e| e.key().clone())
            .filter(|id| !open_issue_ids.contains(&normalize_issue_external_id(id)))
            .collect();
        if !stale.is_empty() {
            for id in &stale {
                self.dispatched.remove(id);
            }
            tracing::debug!(
                count = stale.len(),
                "intake: evicted dispatched entries for closed issues"
            );
            self.persist_dispatched();
        }
    }
}

fn normalize_issue_external_id(external_id: &str) -> String {
    let trimmed = external_id.trim();
    trimmed
        .strip_prefix("issue:")
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(trimmed)
        .to_string()
}

fn dispatched_contains_issue(dispatched: &DashMap<String, TaskId>, issue_id: &str) -> bool {
    dispatched.contains_key(issue_id) || dispatched.contains_key(&format!("issue:{issue_id}"))
}

/// Raw GitHub issue fields returned by the GitHub REST API.
#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    body: Option<String>,
    url: String,
    html_url: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    author_association: Option<String>,
    #[serde(alias = "createdAt")]
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

/// Parsed result from GitHub issue-list output.
struct ParsedGhOutput {
    /// New issues not yet dispatched.
    new_issues: Vec<IncomingIssue>,
    /// All open issue numbers from the API response (for eviction).
    open_issue_ids: HashSet<String>,
}

/// Parse the JSON output of the GitHub issue list API
/// into new issues (filtering out dispatched) and the full set of open issue IDs.
fn parse_gh_output(
    json: &[u8],
    repo: &str,
    dispatched: &DashMap<String, TaskId>,
    project_root: Option<&std::path::Path>,
) -> anyhow::Result<ParsedGhOutput> {
    let issues: Vec<GhIssue> = serde_json::from_slice(json)?;
    let issues: Vec<GhIssue> = issues
        .into_iter()
        .filter(|issue| issue.pull_request.is_none())
        .collect();
    let open_issue_ids: HashSet<String> = issues.iter().map(|i| i.number.to_string()).collect();
    let new_issues = issues
        .into_iter()
        .filter(|issue| {
            let issue_id = issue.number.to_string();
            !dispatched_contains_issue(dispatched, &issue_id)
        })
        .map(|issue| IncomingIssue {
            source: "github".to_string(),
            external_id: issue.number.to_string(),
            identifier: format!("#{}", issue.number),
            title: issue.title,
            description: issue.body,
            repo: Some(repo.to_string()),
            url: Some(issue.html_url.unwrap_or(issue.url)),
            priority: None,
            labels: issue.labels.into_iter().map(|l| l.name).collect(),
            created_at: issue.created_at,
            author_trust_class: classify_author_association(issue.author_association.as_deref()),
            project_root: project_root.map(|p| p.to_path_buf()),
        })
        .collect();
    Ok(ParsedGhOutput {
        new_issues,
        open_issue_ids,
    })
}

fn github_issues_url(api_base_url: &str, repo: &str, label: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(&format!(
        "{}/repos/{repo}/issues",
        api_base_url.trim_end_matches('/')
    ))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("state", "open");
        query.append_pair("per_page", "100");
        if !label.is_empty() {
            query.append_pair("labels", label);
        }
    }
    Ok(url.to_string())
}

fn classify_author_association(author_association: Option<&str>) -> IsolationTrustClass {
    match author_association
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("COLLABORATOR" | "MEMBER" | "OWNER") => IsolationTrustClass::Trusted,
        _ => IsolationTrustClass::NonCollaborator,
    }
}

struct GitHubIssuePage {
    body: Vec<u8>,
    next_url: Option<String>,
}

enum GitHubIssuePageFetch {
    Page(GitHubIssuePage),
    RateLimited(GitHubRateLimit),
}

struct GitHubRateLimit {
    status: StatusCode,
    retry_at: DateTime<Utc>,
}

async fn fetch_github_issue_page(
    client: &reqwest::Client,
    url: &str,
    repo: &str,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubIssuePageFetch> {
    let mut request = client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        if let Some(retry_at) = rate_limit_retry_at(status, response.headers(), Utc::now()) {
            return Ok(GitHubIssuePageFetch::RateLimited(GitHubRateLimit {
                status,
                retry_at,
            }));
        }
        anyhow::bail!(
            "GitHub issue list failed for {repo} at {url} with status {}",
            status
        );
    }
    let next_url = next_link_from_headers(response.headers());
    let body = response.bytes().await?.to_vec();
    Ok(GitHubIssuePageFetch::Page(GitHubIssuePage {
        body,
        next_url,
    }))
}

fn rate_limit_retry_at(
    status: StatusCode,
    headers: &HeaderMap,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let retry_after = retry_after_header_time(headers, now);
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Some(retry_after.unwrap_or_else(|| default_rate_limit_retry_at(now)));
    }

    if status != StatusCode::FORBIDDEN {
        return None;
    }

    if retry_after.is_some() || x_rate_limit_remaining_is_zero(headers) {
        return Some(retry_after.unwrap_or_else(|| {
            x_rate_limit_reset_time(headers).unwrap_or_else(|| default_rate_limit_retry_at(now))
        }));
    }

    None
}

fn retry_after_header_time(headers: &HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    Some(now + chrono::Duration::seconds(seconds.max(0)))
}

fn x_rate_limit_reset_time(headers: &HeaderMap) -> Option<DateTime<Utc>> {
    let epoch_seconds = headers
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()?;
    DateTime::<Utc>::from_timestamp(epoch_seconds, 0)
}

fn x_rate_limit_remaining_is_zero(headers: &HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == "0")
}

fn default_rate_limit_retry_at(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(DEFAULT_RATE_LIMIT_RETRY_SECS)
}

fn next_link_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    parse_next_link(link)
}

fn parse_next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let mut segments = part.split(';').map(str::trim);
        let url_segment = segments.next()?;
        if !url_segment.starts_with('<') || !url_segment.ends_with('>') {
            return None;
        }
        if segments.any(link_segment_has_next_rel) {
            Some(url_segment[1..url_segment.len() - 1].to_string())
        } else {
            None
        }
    })
}

fn link_segment_has_next_rel(segment: &str) -> bool {
    let Some((key, value)) = segment.split_once('=') else {
        return false;
    };
    if !key.trim().eq_ignore_ascii_case("rel") {
        return false;
    }
    value
        .trim()
        .trim_matches('"')
        .split_ascii_whitespace()
        .any(|rel| rel.eq_ignore_ascii_case("next"))
}

#[async_trait]
impl IntakeSource for GitHubIssuesPoller {
    fn name(&self) -> &str {
        "github"
    }

    async fn poll(&self) -> anyhow::Result<Vec<IncomingIssue>> {
        self.poll_from_api_base_url("https://api.github.com").await
    }

    async fn mark_dispatched(&self, external_id: &str, task_id: &TaskId) -> anyhow::Result<()> {
        self.dispatched
            .insert(normalize_issue_external_id(external_id), task_id.clone());
        self.persist_dispatched();
        Ok(())
    }

    async fn unmark_dispatched(&self, external_id: &str) {
        self.dispatched
            .remove(&normalize_issue_external_id(external_id));
        self.persist_dispatched();
    }

    async fn on_task_complete(
        &self,
        external_id: &str,
        result: &TaskCompletionResult,
    ) -> anyhow::Result<()> {
        // Failures that require manual intervention must stay in dispatched so the poller
        // does not immediately re-discover the open issue and hot-loop. The operator must
        // resolve the conflict before the issue can be re-dispatched.
        let needs_manual = result
            .error
            .as_deref()
            .map(|e| e.contains(crate::task_executor::gates::MANUAL_RESOLUTION_REQUIRED))
            .unwrap_or(false);
        let is_workspace_lifecycle = matches!(
            result.failure_kind,
            Some(crate::task_runner::TaskFailureKind::WorkspaceLifecycle)
        );
        // Remove transient failed or cancelled issues from dispatched so the poller can
        // retry them later if they remain open. Done tasks and permanent failures stay
        // dispatched to avoid re-processing.
        if ((result.status.is_failure() && is_workspace_lifecycle) || result.status.is_cancelled())
            && !needs_manual
        {
            self.dispatched
                .remove(&normalize_issue_external_id(external_id));
            self.persist_dispatched();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "github_issues_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "github_issues_rate_limit_tests.rs"]
mod rate_limit_tests;

#[cfg(test)]
#[path = "github_issues_trust_tests.rs"]
mod trust_tests;
