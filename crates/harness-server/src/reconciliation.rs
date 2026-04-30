use crate::http::AppState;
use crate::task_runner::{mutate_and_persist, TaskId, TaskStatus, TaskStore};
use harness_core::config::misc::ReconciliationConfig;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// One candidate task for reconciliation check.
struct Candidate {
    id: TaskId,
    pr_url: Option<String>,
    repo: Option<String>,
    project_root: Option<PathBuf>,
    /// Numeric issue or PR from `external_id` (e.g. `issue:42` → 42).
    issue_num: Option<u64>,
    /// Numeric PR from `external_id` `pr:N` when no `pr_url` is present.
    pr_num_from_ext: Option<u64>,
}

/// A single resolved transition produced by `run_once`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationTransition {
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    /// `false` in dry-run mode.
    pub applied: bool,
}

/// Summary returned by `run_once` and serialised in the HTTP handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub candidates: usize,
    pub skipped_terminal: usize,
    pub transitions: Vec<ReconciliationTransition>,
}

/// External GitHub state observed for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitHubState {
    PrMerged,
    PrClosed,
    IssueClosed,
    Open,
    Unknown,
}

#[derive(Debug, Deserialize)]
struct GitHubPullState {
    state: String,
    merged_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueState {
    state: String,
}

/// Rate-limit state: at most `max_per_minute` GitHub API calls per 60-second window.
struct RateLimiter {
    max_per_minute: u32,
    calls_this_window: u32,
    window_start: Instant,
}

impl RateLimiter {
    fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            calls_this_window: 0,
            window_start: Instant::now(),
        }
    }

    /// Wait if the current window is exhausted, then record one call.
    async fn acquire(&mut self) {
        if self.max_per_minute == 0 {
            return;
        }
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(60) {
            self.window_start = Instant::now();
            self.calls_this_window = 0;
        }
        if self.calls_this_window >= self.max_per_minute {
            let remaining = Duration::from_secs(60).saturating_sub(self.window_start.elapsed());
            if !remaining.is_zero() {
                sleep(remaining).await;
            }
            self.window_start = Instant::now();
            self.calls_this_window = 0;
        }
        self.calls_this_window += 1;
    }
}

/// Try to build a `Candidate` from one `TaskState`.
///
/// Returns `None` when the task is already terminal or has no GitHub reference.
fn candidate_from_task(task: &crate::task_runner::TaskState) -> Option<Candidate> {
    if task.status.is_terminal() {
        return None;
    }
    let (issue_num, pr_num_from_ext) = parse_external_id(task.external_id.as_deref());
    let has_pr = task.pr_url.is_some() || pr_num_from_ext.is_some();
    let has_issue = issue_num.is_some();
    if !has_pr && !has_issue {
        return None;
    }
    Some(Candidate {
        id: task.id.clone(),
        pr_url: task.pr_url.clone(),
        repo: task.repo.clone(),
        project_root: task.project_root.clone(),
        issue_num,
        pr_num_from_ext,
    })
}

/// Collect non-terminal tasks that have a `pr_url` or a parseable `external_id`.
fn collect_candidates(store: &TaskStore) -> (Vec<Candidate>, usize) {
    let mut candidates = Vec::new();
    let mut skipped_terminal = 0usize;

    for entry in store.cache.iter() {
        let task = entry.value();
        if task.status.is_terminal() {
            skipped_terminal += 1;
            continue;
        }
        if let Some(c) = candidate_from_task(task) {
            candidates.push(c);
        }
    }
    (candidates, skipped_terminal)
}

pub(crate) fn parse_external_id(eid: Option<&str>) -> (Option<u64>, Option<u64>) {
    match eid {
        Some(s) if s.starts_with("issue:") => (s["issue:".len()..].parse().ok(), None),
        Some(s) if s.starts_with("pr:") => (None, s["pr:".len()..].parse().ok()),
        _ => (None, None),
    }
}

fn github_api_base_url() -> String {
    std::env::var("HARNESS_GITHUB_API_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

async fn github_get_json<T: DeserializeOwned>(path: &str, github_token: Option<&str>) -> Option<T> {
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!("{}{}", github_api_base_url(), path))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = match tokio::time::timeout(Duration::from_secs(10), request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => response,
        Ok(Ok(response)) => {
            tracing::debug!(status = %response.status(), path, "GitHub state check failed");
            return None;
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, path, "GitHub state check invocation error");
            return None;
        }
        Err(_) => {
            tracing::debug!(path, "GitHub state check timed out after 10s");
            return None;
        }
    };
    response.json::<T>().await.ok()
}

fn classify_pr_state(state: &GitHubPullState) -> GitHubState {
    let merged_at_empty = state.merged_at.as_deref().unwrap_or("").trim().is_empty();
    match (state.state.as_str(), merged_at_empty) {
        ("open", _) | ("OPEN", _) => GitHubState::Open,
        ("merged", _) | ("MERGED", _) | ("closed", false) | ("CLOSED", false) => {
            GitHubState::PrMerged
        }
        ("closed", true) | ("CLOSED", true) => GitHubState::PrClosed,
        _ => GitHubState::Unknown,
    }
}

fn classify_issue_state(state: &GitHubIssueState) -> GitHubState {
    match state.state.as_str() {
        "closed" | "CLOSED" => GitHubState::IssueClosed,
        "open" | "OPEN" => GitHubState::Open,
        _ => GitHubState::Unknown,
    }
}

/// Fetch GitHub PR state from a full URL (e.g. `https://github.com/.../pull/42`).
async fn fetch_pr_state_by_url(pr_url: &str, github_token: Option<&str>) -> GitHubState {
    let Some((owner, repo, pr_number)) = harness_core::prompts::parse_github_pr_url(pr_url) else {
        tracing::debug!(pr_url, "GitHub PR state check skipped for unparseable URL");
        return GitHubState::Unknown;
    };
    fetch_pr_state_by_slug_with_token(&format!("{owner}/{repo}"), pr_number, github_token).await
}

pub(crate) async fn fetch_pr_state_by_slug_with_token(
    repo_slug: &str,
    pr_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    let Some(state) = github_get_json::<GitHubPullState>(
        &format!("/repos/{repo_slug}/pulls/{pr_num}"),
        github_token,
    )
    .await
    else {
        return GitHubState::Unknown;
    };
    classify_pr_state(&state)
}

pub(crate) async fn fetch_issue_state_with_token(
    repo_slug: &str,
    issue_num: u64,
    github_token: Option<&str>,
) -> GitHubState {
    let Some(state) = github_get_json::<GitHubIssueState>(
        &format!("/repos/{repo_slug}/issues/{issue_num}"),
        github_token,
    )
    .await
    else {
        return GitHubState::Unknown;
    };
    classify_issue_state(&state)
}

/// Core reconciliation logic. Callable from the periodic loop and HTTP handler.
pub async fn run_once(
    store: &Arc<TaskStore>,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
) -> ReconciliationReport {
    run_once_with_token(store, max_gh_calls_per_minute, dry_run, None).await
}

pub async fn run_once_with_token(
    store: &Arc<TaskStore>,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
    github_token: Option<&str>,
) -> ReconciliationReport {
    let (candidates, skipped_terminal) = collect_candidates(store);
    let mut rate = RateLimiter::new(max_gh_calls_per_minute);
    let mut repo_slug_cache = HashMap::new();
    let mut transitions = Vec::new();

    for candidate in &candidates {
        let gh_state =
            resolve_github_state(candidate, &mut rate, &mut repo_slug_cache, github_token).await;

        let new_status = transition_for_github_state(gh_state);

        let Some((target_status, reason)) = new_status else {
            continue;
        };

        // Get current status for the transition record.
        let from_status = store
            .cache
            .get(&candidate.id)
            .map(|e| e.status.as_ref().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let applied = if dry_run {
            false
        } else {
            apply_transition(store, &candidate.id, target_status.clone(), reason).await
        };

        if !dry_run && applied {
            store.abort_task(&candidate.id);
        }

        transitions.push(ReconciliationTransition {
            task_id: candidate.id.0.clone(),
            from: from_status,
            to: target_status.as_ref().to_string(),
            reason: reason.to_string(),
            applied,
        });
    }

    let total_candidates = candidates.len() + skipped_terminal;
    ReconciliationReport {
        candidates: total_candidates,
        skipped_terminal,
        transitions,
    }
}

fn transition_for_github_state(gh_state: GitHubState) -> Option<(TaskStatus, &'static str)> {
    match gh_state {
        GitHubState::PrMerged => Some((TaskStatus::Done, "reconciled: PR merged externally")),
        GitHubState::PrClosed => Some((TaskStatus::Cancelled, "reconciled: PR closed externally")),
        GitHubState::IssueClosed => {
            Some((TaskStatus::Cancelled, "reconciled: issue closed before PR"))
        }
        GitHubState::Open | GitHubState::Unknown => None,
    }
}

/// Determine the current GitHub state for one candidate, consuming rate-limit budget.
async fn resolve_github_state(
    candidate: &Candidate,
    rate: &mut RateLimiter,
    repo_slug_cache: &mut HashMap<PathBuf, Option<String>>,
    github_token: Option<&str>,
) -> GitHubState {
    // PR URL takes precedence — most candidates in `implementing`/`reviewing`
    // will have one.
    if let Some(pr_url) = &candidate.pr_url {
        rate.acquire().await;
        return fetch_pr_state_by_url(pr_url, github_token).await;
    }
    let repo_slug = resolve_repo_slug(candidate, repo_slug_cache).await;
    let Some(repo_slug) = repo_slug else {
        tracing::debug!(
            task_id = %candidate.id.0,
            "GitHub state check skipped because repository slug is unavailable"
        );
        return GitHubState::Unknown;
    };
    if let Some(pr_num) = candidate.pr_num_from_ext {
        rate.acquire().await;
        return fetch_pr_state_by_slug_with_token(&repo_slug, pr_num, github_token).await;
    }
    if let Some(issue_num) = candidate.issue_num {
        rate.acquire().await;
        return fetch_issue_state_with_token(&repo_slug, issue_num, github_token).await;
    }
    GitHubState::Unknown
}

async fn resolve_repo_slug(
    candidate: &Candidate,
    repo_slug_cache: &mut HashMap<PathBuf, Option<String>>,
) -> Option<String> {
    match candidate.repo.as_deref() {
        Some(repo) if !repo.trim().is_empty() => Some(repo.to_string()),
        _ => match candidate.project_root.as_ref() {
            Some(project_root) => {
                if let Some(cached) = cached_repo_slug(repo_slug_cache, project_root) {
                    return cached.clone();
                }

                let detected =
                    crate::task_executor::pr_detection::detect_repo_slug(project_root).await;
                repo_slug_cache.insert(project_root.clone(), detected.clone());
                detected
            }
            None => None,
        },
    }
}

fn cached_repo_slug(
    repo_slug_cache: &HashMap<PathBuf, Option<String>>,
    project_root: &PathBuf,
) -> Option<Option<String>> {
    repo_slug_cache.get(project_root).cloned()
}

/// Apply a status transition to a task, returning `true` on success.
async fn apply_transition(
    store: &Arc<TaskStore>,
    task_id: &TaskId,
    new_status: TaskStatus,
    reason: &str,
) -> bool {
    let result = mutate_and_persist(store, task_id, |s| {
        // TOCTOU guard: skip if already terminal (e.g. completed between
        // candidate collection and now).
        if s.status.is_terminal() {
            return;
        }
        tracing::info!(
            task_id = %task_id.0,
            from = s.status.as_ref(),
            to = new_status.as_ref(),
            reason,
            "reconciliation: applying transition"
        );
        s.status = new_status.clone();
        s.scheduler.mark_terminal(&new_status);
    })
    .await;

    match result {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(task_id = %task_id.0, "reconciliation: persist failed: {e}");
            false
        }
    }
}

/// Spawn the periodic reconciliation loop as a background task.
///
/// Returns immediately without spawning when `config.enabled` is false.
pub fn start(state: Arc<AppState>, config: ReconciliationConfig) {
    if !config.enabled {
        tracing::debug!("reconciliation: periodic loop disabled");
        return;
    }

    tokio::spawn(async move {
        reconciliation_loop(state, config).await;
    });
}

async fn reconciliation_loop(state: Arc<AppState>, config: ReconciliationConfig) {
    let interval = Duration::from_secs(config.interval_secs);
    // Brief init delay so the server is fully up before the first tick.
    sleep(Duration::from_secs(15)).await;

    loop {
        let report = run_once_with_token(
            &state.core.tasks,
            config.max_gh_calls_per_minute,
            false,
            state.core.server.config.server.github_token.as_deref(),
        )
        .await;
        record_repo_backlog_reconciliation_transitions(&state, &report).await;

        // Clean up workspaces for tasks that were just terminated by reconciliation,
        // so the workspace is gone within the same tick (issue #969).
        if let Some(ref wmgr) = state.concurrency.workspace_mgr {
            let transitioned_ids: Vec<crate::task_runner::TaskId> = report
                .transitions
                .iter()
                .filter(|t| t.applied)
                .map(|t| harness_core::types::TaskId(t.task_id.clone()))
                .collect();
            if !transitioned_ids.is_empty() {
                if let Err(e) = wmgr.cleanup_terminal(&transitioned_ids).await {
                    tracing::warn!("reconciliation: workspace cleanup failed: {e}");
                }
            }
        }

        tracing::info!(
            candidates = report.candidates,
            skipped_terminal = report.skipped_terminal,
            transitions = report.transitions.len(),
            "reconciliation: tick complete"
        );
        sleep(interval).await;
    }
}

async fn record_repo_backlog_reconciliation_transitions(
    state: &Arc<AppState>,
    report: &ReconciliationReport,
) {
    for transition in &report.transitions {
        if !transition.applied || transition.reason != "reconciled: PR merged externally" {
            continue;
        }
        let task_id = harness_core::types::TaskId(transition.task_id.clone());
        let Some(task) = state.core.tasks.get(&task_id) else {
            continue;
        };
        let Some(pr_number) = task
            .pr_url
            .as_deref()
            .and_then(harness_core::prompts::parse_github_pr_url)
            .map(|(_, _, pr_number)| pr_number)
            .or_else(|| parse_external_id(task.external_id.as_deref()).1)
        else {
            continue;
        };
        let issue_number = task
            .issue
            .or_else(|| parse_external_id(task.external_id.as_deref()).0);
        let project_root = task
            .project_root
            .as_deref()
            .unwrap_or(&state.core.project_root);
        crate::workflow_runtime_repo_backlog::record_merged_pr(
            state.core.workflow_runtime_store.as_deref(),
            state.core.issue_workflow_store.as_deref(),
            crate::workflow_runtime_repo_backlog::MergedPrRuntimeContext {
                project_root,
                repo: task.repo.as_deref(),
                issue_number,
                pr_number,
                pr_url: task.pr_url.as_deref(),
                detail: &transition.reason,
            },
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::TaskState;
    use harness_core::types::TaskId;
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    fn make_task(
        id: &str,
        status: TaskStatus,
        pr_url: Option<&str>,
        external_id: Option<&str>,
    ) -> TaskState {
        let tid = TaskId(id.to_string());
        let mut task = TaskState::new(tid);
        task.status = status;
        task.pr_url = pr_url.map(|s| s.to_string());
        task.external_id = external_id.map(|s| s.to_string());
        task
    }

    fn async_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct ScopedEnvVar {
        key: String,
        original: Option<String>,
    }

    impl ScopedEnvVar {
        fn set(key: &str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self {
                key: key.to_string(),
                original,
            }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                unsafe { std::env::set_var(&self.key, value) };
            } else {
                unsafe { std::env::remove_var(&self.key) };
            }
        }
    }

    async fn github_state_server(routes: Vec<(&'static str, &'static str)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind GitHub mock");
        let addr = listener.local_addr().expect("GitHub mock address");
        let routes: HashMap<String, &'static str> = routes
            .into_iter()
            .map(|(path, body)| (path.to_string(), body))
            .collect();

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0_u8; 2048];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let request_line = request.lines().next().unwrap_or_default();
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let (status, response_body) = match routes.get(&path).copied() {
                        Some(body) => ("200 OK", body),
                        None => ("404 Not Found", "{}"),
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        format!("http://{addr}")
    }

    fn write_git_remote_config(path: &std::path::Path, origin: &str) {
        let dotgit = path.join(".git");
        std::fs::create_dir_all(&dotgit).expect("create .git");
        std::fs::write(
            dotgit.join("config"),
            format!("[remote \"origin\"]\n\turl = {origin}\n"),
        )
        .expect("write git config");
    }

    // ── Pure function tests (no DB required) ─────────────────────────────

    #[test]
    fn parse_external_id_issue() {
        assert_eq!(parse_external_id(Some("issue:42")), (Some(42), None));
    }

    #[test]
    fn parse_external_id_pr() {
        assert_eq!(parse_external_id(Some("pr:7")), (None, Some(7)));
    }

    #[test]
    fn parse_external_id_none() {
        assert_eq!(parse_external_id(None), (None, None));
    }

    #[test]
    fn candidate_from_task_skips_terminal() {
        let mut t = make_task(
            "x",
            TaskStatus::Done,
            Some("https://github.com/a/b/pull/1"),
            None,
        );
        assert!(candidate_from_task(&t).is_none());
        t.status = TaskStatus::Cancelled;
        assert!(candidate_from_task(&t).is_none());
    }

    #[test]
    fn candidate_from_task_skips_no_refs() {
        let t = make_task("x", TaskStatus::Implementing, None, None);
        assert!(candidate_from_task(&t).is_none());
    }

    #[test]
    fn candidate_from_task_picks_pr_url() {
        let t = make_task(
            "x",
            TaskStatus::Implementing,
            Some("https://github.com/a/b/pull/2"),
            None,
        );
        let c = candidate_from_task(&t).unwrap();
        assert!(c.pr_url.is_some());
        assert_eq!(c.issue_num, None);
    }

    #[test]
    fn candidate_from_task_carries_project_root() {
        let mut t = make_task("x", TaskStatus::Pending, None, Some("issue:9"));
        t.project_root = Some(PathBuf::from("/tmp/projects/alpha"));
        let c = candidate_from_task(&t).unwrap();
        assert_eq!(c.project_root, Some(PathBuf::from("/tmp/projects/alpha")));
    }

    #[test]
    fn candidate_from_task_picks_issue_external_id() {
        let t = make_task("x", TaskStatus::Pending, None, Some("issue:9"));
        let c = candidate_from_task(&t).unwrap();
        assert_eq!(c.issue_num, Some(9));
        assert!(c.pr_url.is_none());
    }

    #[tokio::test]
    async fn resolve_repo_slug_uses_candidate_project_root_when_repo_missing() {
        let repo_a = tempfile::tempdir().expect("repo a tempdir");
        let repo_b = tempfile::tempdir().expect("repo b tempdir");
        write_git_remote_config(repo_a.path(), "https://github.com/example/repo-a.git");
        write_git_remote_config(repo_b.path(), "https://github.com/example/repo-b.git");
        assert_eq!(
            crate::task_executor::pr_detection::detect_repo_slug(repo_a.path()).await,
            Some("example/repo-a".to_string())
        );

        let candidate = Candidate {
            id: TaskId("task-1".to_string()),
            pr_url: None,
            repo: None,
            project_root: Some(repo_b.path().to_path_buf()),
            issue_num: Some(9),
            pr_num_from_ext: None,
        };

        let mut cache = HashMap::new();
        let repo_slug = resolve_repo_slug(&candidate, &mut cache).await;
        assert_eq!(repo_slug, Some("example/repo-b".to_string()));
    }

    #[tokio::test]
    async fn run_once_uses_each_task_project_root_when_repo_is_missing() {
        let _env_guard = async_env_lock().lock().await;
        if !crate::test_helpers::db_tests_enabled().await {
            return;
        }
        let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
        let repo_a = tempfile::tempdir().expect("repo a tempdir");
        let repo_b = tempfile::tempdir().expect("repo b tempdir");
        write_git_remote_config(repo_a.path(), "https://github.com/example/repo-a.git");
        write_git_remote_config(repo_b.path(), "https://github.com/example/repo-b.git");

        let api_base = github_state_server(vec![
            ("/repos/example/repo-a/issues/9", r#"{"state":"open"}"#),
            ("/repos/example/repo-a/issues/41", r#"{"state":"open"}"#),
            ("/repos/example/repo-b/issues/9", r#"{"state":"closed"}"#),
        ])
        .await;
        let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

        let dir = tempfile::tempdir().expect("task store tempdir");
        let store = match TaskStore::open(&dir.path().join("tasks.db")).await {
            Ok(store) => store,
            Err(err) if crate::test_helpers::is_pool_timeout(&err) => return,
            Err(err) => panic!("open task store: {err}"),
        };

        let mut repo_task = make_task("repo-task", TaskStatus::Pending, None, Some("issue:41"));
        repo_task.repo = Some("example/repo-a".to_string());
        repo_task.project_root = Some(repo_a.path().to_path_buf());
        store.insert(&repo_task).await;

        let mut repo_less_task =
            make_task("repo-less-task", TaskStatus::Pending, None, Some("issue:9"));
        repo_less_task.project_root = Some(repo_b.path().to_path_buf());
        store.insert(&repo_less_task).await;

        let report = run_once(&store, 20, false).await;

        assert_eq!(report.transitions.len(), 1);
        assert_eq!(report.transitions[0].task_id, repo_less_task.id.0);
        assert_eq!(
            report.transitions[0].reason,
            "reconciled: issue closed before PR"
        );

        let repo_task_after = store.get(&repo_task.id).expect("repo task remains");
        assert_eq!(repo_task_after.status, TaskStatus::Pending);

        let repo_less_after = store
            .get(&repo_less_task.id)
            .expect("repo-less task remains");
        assert_eq!(repo_less_after.status, TaskStatus::Cancelled);
    }

    #[test]
    fn classify_pr_state_handles_merged_and_closed() {
        assert_eq!(
            classify_pr_state(&GitHubPullState {
                state: "closed".to_string(),
                merged_at: Some("2024-01-01T00:00:00Z".to_string()),
            }),
            GitHubState::PrMerged
        );
        assert_eq!(
            classify_pr_state(&GitHubPullState {
                state: "closed".to_string(),
                merged_at: None,
            }),
            GitHubState::PrClosed
        );
    }

    #[test]
    fn classify_issue_state_handles_open_and_closed() {
        assert_eq!(
            classify_issue_state(&GitHubIssueState {
                state: "open".to_string(),
            }),
            GitHubState::Open
        );
        assert_eq!(
            classify_issue_state(&GitHubIssueState {
                state: "closed".to_string(),
            }),
            GitHubState::IssueClosed
        );
    }

    #[test]
    fn transition_mapping_matches_external_states() {
        assert_eq!(
            transition_for_github_state(GitHubState::PrMerged),
            Some((TaskStatus::Done, "reconciled: PR merged externally"))
        );
        assert_eq!(
            transition_for_github_state(GitHubState::PrClosed),
            Some((TaskStatus::Cancelled, "reconciled: PR closed externally"))
        );
        assert_eq!(transition_for_github_state(GitHubState::Open), None);
    }

    // Reconciliation payload guard: ReconciliationReport and
    // ReconciliationTransition are serialised over HTTP.  They must never
    // contain a UUID workspace path.
    #[test]
    fn reconciliation_payload_has_no_workspace_paths() {
        let report = ReconciliationReport {
            candidates: 3,
            skipped_terminal: 1,
            transitions: vec![ReconciliationTransition {
                task_id: "task-abc123".to_string(),
                from: "implementing".to_string(),
                to: "done".to_string(),
                reason: "PR merged".to_string(),
                applied: true,
            }],
        };
        let json =
            serde_json::to_string(&report).expect("ReconciliationReport must serialise to JSON");
        assert!(
            !json.contains("/workspaces/"),
            "ReconciliationReport JSON must not contain a workspace path, got: {json}"
        );
    }
}
