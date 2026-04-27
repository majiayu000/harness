use crate::http::AppState;
use crate::task_runner::{mutate_and_persist, TaskId, TaskStatus, TaskStore};
use harness_core::config::misc::ReconciliationConfig;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// One candidate task for reconciliation check.
struct Candidate {
    id: TaskId,
    pr_url: Option<String>,
    repo: Option<String>,
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

async fn github_get_json<T: DeserializeOwned>(path: &str) -> Option<T> {
    let client = reqwest::Client::new();
    let mut request = client
        .get(format!("{}{}", github_api_base_url(), path))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "harness-server");
    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        if !token.trim().is_empty() {
            request = request.bearer_auth(token);
        }
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
async fn fetch_pr_state_by_url(pr_url: &str) -> GitHubState {
    let Some((owner, repo, pr_number)) = harness_core::prompts::parse_github_pr_url(pr_url) else {
        tracing::debug!(pr_url, "GitHub PR state check skipped for unparseable URL");
        return GitHubState::Unknown;
    };
    fetch_pr_state_by_slug(&format!("{owner}/{repo}"), pr_number).await
}

/// Fetch GitHub PR state by repository slug and number.
async fn fetch_pr_state_by_slug(repo_slug: &str, pr_num: u64) -> GitHubState {
    let Some(state) =
        github_get_json::<GitHubPullState>(&format!("/repos/{repo_slug}/pulls/{pr_num}")).await
    else {
        return GitHubState::Unknown;
    };
    classify_pr_state(&state)
}

/// Fetch issue state by repository slug and number.
async fn fetch_issue_state(repo_slug: &str, issue_num: u64) -> GitHubState {
    let Some(state) =
        github_get_json::<GitHubIssueState>(&format!("/repos/{repo_slug}/issues/{issue_num}"))
            .await
    else {
        return GitHubState::Unknown;
    };
    classify_issue_state(&state)
}

/// Core reconciliation logic. Callable from the periodic loop and HTTP handler.
pub async fn run_once(
    store: &Arc<TaskStore>,
    project: &Path,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
) -> ReconciliationReport {
    let (candidates, skipped_terminal) = collect_candidates(store);
    let mut rate = RateLimiter::new(max_gh_calls_per_minute);
    let mut transitions = Vec::new();

    for candidate in &candidates {
        let gh_state = resolve_github_state(candidate, project, &mut rate).await;

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
    project: &Path,
    rate: &mut RateLimiter,
) -> GitHubState {
    // PR URL takes precedence — most candidates in `implementing`/`reviewing`
    // will have one.
    if let Some(pr_url) = &candidate.pr_url {
        rate.acquire().await;
        return fetch_pr_state_by_url(pr_url).await;
    }
    let repo_slug = match candidate.repo.as_deref() {
        Some(repo) if !repo.trim().is_empty() => Some(repo.to_string()),
        _ => crate::task_executor::pr_detection::detect_repo_slug(project).await,
    };
    let Some(repo_slug) = repo_slug else {
        tracing::debug!(
            task_id = %candidate.id.0,
            "GitHub state check skipped because repository slug is unavailable"
        );
        return GitHubState::Unknown;
    };
    if let Some(pr_num) = candidate.pr_num_from_ext {
        rate.acquire().await;
        return fetch_pr_state_by_slug(&repo_slug, pr_num).await;
    }
    if let Some(issue_num) = candidate.issue_num {
        rate.acquire().await;
        return fetch_issue_state(&repo_slug, issue_num).await;
    }
    GitHubState::Unknown
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
        let report = run_once(
            &state.core.tasks,
            &state.core.project_root,
            config.max_gh_calls_per_minute,
            false,
        )
        .await;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::TaskState;
    use harness_core::types::TaskId;

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
    fn candidate_from_task_picks_issue_external_id() {
        let t = make_task("x", TaskStatus::Pending, None, Some("issue:9"));
        let c = candidate_from_task(&t).unwrap();
        assert_eq!(c.issue_num, Some(9));
        assert!(c.pr_url.is_none());
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
