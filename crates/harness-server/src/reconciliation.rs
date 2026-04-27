use crate::http::AppState;
use crate::task_runner::{mutate_and_persist, TaskId, TaskStatus, TaskStore};
use harness_core::config::misc::ReconciliationConfig;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::sleep;

/// One candidate task for reconciliation check.
struct Candidate {
    id: TaskId,
    pr_url: Option<String>,
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

/// Rate-limit state: at most `max_per_minute` `gh` calls per 60-second window.
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

/// Fetch GitHub PR state from a full URL (e.g. `https://github.com/…/pull/42`).
async fn fetch_pr_state_by_url(gh_bin: &str, pr_url: &str, project: &Path) -> GitHubState {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(gh_bin)
            .current_dir(project)
            .args([
                "pr",
                "view",
                pr_url,
                "--json",
                "state,mergedAt",
                "--jq",
                ".state + \"|\" + ((.mergedAt // \"\")|tostring)",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;

    classify_pr_output(result)
}

/// Fetch GitHub PR state by number.
pub(crate) async fn fetch_pr_state_by_num(
    gh_bin: &str,
    pr_num: u64,
    project: &Path,
) -> GitHubState {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(gh_bin)
            .current_dir(project)
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "state,mergedAt",
                "--jq",
                ".state + \"|\" + ((.mergedAt // \"\")|tostring)",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;

    classify_pr_output(result)
}

fn classify_pr_output(
    result: Result<Result<std::process::Output, std::io::Error>, tokio::time::error::Elapsed>,
) -> GitHubState {
    let output = match result {
        Ok(Ok(o)) if o.status.success() => o,
        Ok(Ok(o)) => {
            tracing::debug!(exit = ?o.status.code(), "reconciliation: gh pr view failed");
            return GitHubState::Unknown;
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "reconciliation: gh pr view invocation error");
            return GitHubState::Unknown;
        }
        Err(_) => {
            tracing::debug!("reconciliation: gh pr view timed out");
            return GitHubState::Unknown;
        }
    };
    let raw = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();
    let (state, merged_at) = raw.split_once('|').unwrap_or((raw.as_str(), ""));
    match (state.trim(), merged_at.trim().is_empty()) {
        ("OPEN", _) => GitHubState::Open,
        ("MERGED", _) | ("CLOSED", false) => GitHubState::PrMerged,
        ("CLOSED", true) => GitHubState::PrClosed,
        _ => GitHubState::Unknown,
    }
}

/// Fetch issue state by number.
pub(crate) async fn fetch_issue_state(gh_bin: &str, issue_num: u64, project: &Path) -> GitHubState {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(gh_bin)
            .current_dir(project)
            .args([
                "issue",
                "view",
                &issue_num.to_string(),
                "--json",
                "state",
                "--jq",
                ".state",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match result {
        Ok(Ok(o)) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_uppercase();
            match s.as_str() {
                "CLOSED" => GitHubState::IssueClosed,
                "OPEN" => GitHubState::Open,
                _ => GitHubState::Unknown,
            }
        }
        Ok(Ok(o)) => {
            tracing::debug!(exit = ?o.status.code(), "reconciliation: gh issue view failed");
            GitHubState::Unknown
        }
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "reconciliation: gh issue view invocation error");
            GitHubState::Unknown
        }
        Err(_) => {
            tracing::debug!("reconciliation: gh issue view timed out");
            GitHubState::Unknown
        }
    }
}

/// Core reconciliation logic. Callable from the periodic loop, the HTTP
/// handler, and test code. `gh_bin` is injectable for testing.
pub async fn run_once_with_gh(
    store: &Arc<TaskStore>,
    project: &Path,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
    gh_bin: &str,
) -> ReconciliationReport {
    let (candidates, skipped_terminal) = collect_candidates(store);
    let mut rate = RateLimiter::new(max_gh_calls_per_minute);
    let mut transitions = Vec::new();

    for candidate in &candidates {
        let gh_state = resolve_github_state(gh_bin, candidate, project, &mut rate).await;

        let new_status = match gh_state {
            GitHubState::PrMerged => Some((TaskStatus::Done, "reconciled: PR merged externally")),
            GitHubState::PrClosed => {
                Some((TaskStatus::Cancelled, "reconciled: PR closed externally"))
            }
            GitHubState::IssueClosed => {
                Some((TaskStatus::Cancelled, "reconciled: issue closed before PR"))
            }
            GitHubState::Open | GitHubState::Unknown => None,
        };

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

/// Determine the current GitHub state for one candidate, consuming rate-limit budget.
async fn resolve_github_state(
    gh_bin: &str,
    candidate: &Candidate,
    project: &Path,
    rate: &mut RateLimiter,
) -> GitHubState {
    // PR URL takes precedence — most candidates in `implementing`/`reviewing`
    // will have one.
    if let Some(pr_url) = &candidate.pr_url {
        rate.acquire().await;
        return fetch_pr_state_by_url(gh_bin, pr_url, project).await;
    }
    if let Some(pr_num) = candidate.pr_num_from_ext {
        rate.acquire().await;
        return fetch_pr_state_by_num(gh_bin, pr_num, project).await;
    }
    if let Some(issue_num) = candidate.issue_num {
        rate.acquire().await;
        return fetch_issue_state(gh_bin, issue_num, project).await;
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

/// Public entry point for callers that always use the real `gh` binary.
pub async fn run_once(
    store: &Arc<TaskStore>,
    project: &Path,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
) -> ReconciliationReport {
    run_once_with_gh(store, project, max_gh_calls_per_minute, dry_run, "gh").await
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
    use harness_core::config::dirs::default_db_path;
    use harness_core::types::TaskId;

    fn write_mock_gh(dir: &tempfile::TempDir, stdout: &str) -> std::path::PathBuf {
        let script = dir.path().join("gh");
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{stdout}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    fn write_mock_gh_fail(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    async fn open_store(dir: &tempfile::TempDir) -> Option<Arc<crate::task_runner::TaskStore>> {
        if !crate::test_helpers::db_tests_enabled().await {
            return None;
        }
        let db_path = default_db_path(dir.path(), "tasks");
        crate::task_runner::TaskStore::open(&db_path).await.ok()
    }

    fn populate(store: &Arc<crate::task_runner::TaskStore>, tasks: Vec<TaskState>) {
        for task in tasks {
            store.cache.insert(task.id.clone(), task);
        }
    }

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

    // ── DB-gated integration tests ────────────────────────────────────────

    #[tokio::test]
    async fn pr_merged_transitions_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task(
                "t1",
                TaskStatus::Implementing,
                Some("https://github.com/acme/repo/pull/1"),
                None,
            )],
        );
        let gh = write_mock_gh(&dir, "MERGED|2024-01-01T00:00:00Z");
        let report = run_once_with_gh(&store, dir.path(), 60, false, gh.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 1);
        assert_eq!(report.transitions[0].to, "done");
        assert!(report.transitions[0].applied);
        assert_eq!(
            store.cache.get(&TaskId("t1".into())).unwrap().status,
            TaskStatus::Done
        );
    }

    #[tokio::test]
    async fn pr_closed_no_merge_transitions_to_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task(
                "t2",
                TaskStatus::Reviewing,
                Some("https://github.com/acme/repo/pull/2"),
                None,
            )],
        );
        let gh = write_mock_gh(&dir, "CLOSED|");
        let report = run_once_with_gh(&store, dir.path(), 60, false, gh.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 1);
        assert_eq!(report.transitions[0].to, "cancelled");
        assert!(report.transitions[0].applied);
    }

    #[tokio::test]
    async fn pr_closed_with_merged_at_treats_as_done() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task(
                "t3",
                TaskStatus::Implementing,
                Some("https://github.com/acme/repo/pull/3"),
                None,
            )],
        );
        let gh = write_mock_gh(&dir, "CLOSED|2024-02-01T00:00:00Z");
        let report = run_once_with_gh(&store, dir.path(), 60, false, gh.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 1);
        assert_eq!(report.transitions[0].to, "done");
    }

    #[tokio::test]
    async fn issue_only_task_closed_transitions_to_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task("t4", TaskStatus::Pending, None, Some("issue:7"))],
        );
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'CLOSED\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let report =
            run_once_with_gh(&store, dir.path(), 60, false, script.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 1);
        assert_eq!(report.transitions[0].to, "cancelled");
    }

    #[tokio::test]
    async fn terminal_task_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task(
                "t5",
                TaskStatus::Done,
                Some("https://github.com/acme/repo/pull/5"),
                None,
            )],
        );
        let gh = write_mock_gh(&dir, "MERGED|2024-01-01T00:00:00Z");
        let report = run_once_with_gh(&store, dir.path(), 60, false, gh.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 0);
        assert_eq!(report.skipped_terminal, 1);
    }

    #[tokio::test]
    async fn gh_timeout_leaves_task_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task(
                "t6",
                TaskStatus::Implementing,
                Some("https://github.com/acme/repo/pull/6"),
                None,
            )],
        );
        let script = dir.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let report = tokio::time::timeout(
            Duration::from_secs(15),
            run_once_with_gh(&store, dir.path(), 60, false, script.to_str().unwrap()),
        )
        .await
        .expect("run_once should complete within 15s");
        assert_eq!(report.transitions.len(), 0);
        assert_eq!(
            store.cache.get(&TaskId("t6".into())).unwrap().status,
            TaskStatus::Implementing
        );
    }

    #[tokio::test]
    async fn dry_run_reports_but_does_not_apply() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        populate(
            &store,
            vec![make_task(
                "t7",
                TaskStatus::Implementing,
                Some("https://github.com/acme/repo/pull/7"),
                None,
            )],
        );
        let gh = write_mock_gh(&dir, "MERGED|2024-01-01T00:00:00Z");
        let report = run_once_with_gh(&store, dir.path(), 60, true, gh.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 1);
        assert!(!report.transitions[0].applied);
        assert_eq!(
            store.cache.get(&TaskId("t7".into())).unwrap().status,
            TaskStatus::Implementing
        );
    }

    #[tokio::test]
    async fn rate_limiter_no_panic_with_max_two() {
        let dir = tempfile::tempdir().unwrap();
        let Some(store) = open_store(&dir).await else {
            return;
        };
        for i in 1u32..=3 {
            populate(
                &store,
                vec![make_task(
                    &format!("rl-{i}"),
                    TaskStatus::Implementing,
                    Some(&format!("https://github.com/acme/repo/pull/{i}")),
                    None,
                )],
            );
        }
        let gh = write_mock_gh_fail(&dir);
        let report = run_once_with_gh(&store, dir.path(), 2, false, gh.to_str().unwrap()).await;
        assert_eq!(report.transitions.len(), 0);
        assert_eq!(report.candidates, 3);
    }
}
