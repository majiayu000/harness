use super::types::{CreateTaskRequest, TaskId};
use harness_core::types::{Decision, Event, SessionId};
use std::path::PathBuf;

/// Only persist structured safe labels — never raw prompt text.
pub(crate) fn summarize_request_description(req: &CreateTaskRequest) -> Option<String> {
    if let Some(n) = req.issue {
        return Some(format!("issue #{n}"));
    }
    if let Some(n) = req.pr {
        return Some(format!("PR #{n}"));
    }
    // Prompt-only tasks: store a generic label so that:
    //   (a) sibling-awareness can include them (prevents parallel agents stomping the same files),
    //   (b) operators can identify crashed tasks in the DB/dashboard after a restart.
    // The prompt itself is deliberately not stored.
    if req.prompt.is_some() {
        return Some("prompt task".to_string());
    }
    None
}

pub(crate) async fn fill_missing_repo_from_project(req: &mut CreateTaskRequest) {
    if req.repo.is_some() {
        return;
    }
    let Some(project) = req.project.as_deref() else {
        return;
    };
    req.repo = crate::task_executor::pr_detection::detect_repo_slug(project).await;
}

/// Detect the main git worktree root using a blocking subprocess call.
/// Must be called via `tokio::task::spawn_blocking` in async contexts.
pub(crate) fn detect_main_worktree() -> PathBuf {
    std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
                .map(|p| PathBuf::from(p.trim()))
        })
        .unwrap_or_else(|| {
            tracing::warn!(
                "detect_main_worktree: could not detect git worktree root, falling back to '.'"
            );
            PathBuf::from(".")
        })
}

/// Convert a user-facing timeout value to a Duration.
/// `0` means "no timeout" and maps to ~277 hours (effectively unlimited).
pub(crate) fn effective_turn_timeout(secs: u64) -> tokio::time::Duration {
    if secs == 0 {
        tokio::time::Duration::from_secs(999_999)
    } else {
        tokio::time::Duration::from_secs(secs)
    }
}

fn describe_detect_main_worktree_join_error(join_err: &tokio::task::JoinError) -> String {
    if join_err.is_panic() {
        format!("detect_main_worktree panicked: {join_err}")
    } else if join_err.is_cancelled() {
        format!("detect_main_worktree was cancelled: {join_err}")
    } else {
        format!("detect_main_worktree failed: {join_err}")
    }
}

pub(crate) async fn resolve_project_root_with(
    requested_project: Option<PathBuf>,
    detect_worktree: impl FnOnce() -> PathBuf + Send + 'static,
) -> anyhow::Result<PathBuf> {
    match requested_project {
        Some(project) => Ok(project),
        None => tokio::task::spawn_blocking(detect_worktree)
            .await
            .map_err(|join_err| {
                let reason = describe_detect_main_worktree_join_error(&join_err);
                tracing::error!("{reason}");
                anyhow::anyhow!("{reason}")
            }),
    }
}

/// Resolve and canonicalize the project root so the caller can obtain a stable
/// semaphore key before acquiring the concurrency permit.
///
/// When `project` is `None` the main git worktree is detected automatically.
/// Symlinks, relative paths and the `None` sentinel all converge to the same
/// canonical `PathBuf`, preventing the same repository from landing in
/// different per-project buckets due to path aliasing.
///
/// This function only resolves symlinks — it does NOT enforce the HOME-boundary
/// restriction applied by `validate_project_root`. Full validation still
/// happens inside `spawn_task` once the task is running.
pub(crate) async fn resolve_canonical_project(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = resolve_project_root_with(project, detect_main_worktree).await?;
    // Best-effort canonicalize: if the path doesn't exist yet (e.g. in tests
    // using a path that will be created later) fall back to the raw path so
    // we at least get a consistent string key.
    Ok(raw.canonicalize().unwrap_or(raw))
}

pub(crate) async fn log_task_failure_event(
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: &str,
) {
    let mut event = Event::new(
        SessionId::new(),
        "task_failure",
        "task_runner",
        Decision::Block,
    );
    event.reason = Some(reason.to_string());
    event.detail = Some(format!("task_id={}", task_id.0));
    if let Err(e) = events.log(&event).await {
        tracing::warn!("failed to log task_failure event for {task_id:?}: {e}");
    }
}

/// Patterns that indicate a transient (retryable) failure rather than a permanent one.
const TRANSIENT_PATTERNS: &[&str] = &[
    "at capacity",
    "rate limit",
    "rate_limit",
    "hit your limit",
    "429",
    "502 Bad Gateway",
    "503 Service",
    "overloaded",
    "connection reset",
    "connection refused",
    "broken pipe",
    "EOF",
    "stream idle timeout",
    "stream stall",
    "ECONNRESET",
    "ETIMEDOUT",
    // SQLite transient contention — SQLITE_BUSY / SQLITE_LOCKED
    "database is locked",
    "database table is locked",
    "SQLITE_BUSY",
    "SQLITE_LOCKED",
];

/// Pattern indicating the CLI account-level usage limit has been reached.
/// When detected, the global rate-limit circuit breaker is activated to
/// prevent all other tasks from wasting turns.
pub(crate) const ACCOUNT_LIMIT_PATTERN: &str = "hit your limit";

/// Maximum number of automatic retries for transient failures.
pub(crate) const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Return `true` when a free-text prompt is complex enough to require a Plan phase
/// before implementation.
///
/// Heuristic: prompt longer than 200 words OR contains 3 or more file-path-like
/// tokens (sequences containing `/` or ending in a recognised source extension).
pub(crate) fn prompt_requires_plan(prompt: &str) -> bool {
    let word_count = prompt.split_whitespace().count();
    if word_count > 200 {
        return true;
    }
    let file_path_count = prompt
        .split_whitespace()
        .filter(|tok| {
            // Exclude XML/HTML tags — `</foo>` contains '/' but is not a file path.
            // System prompts wrap user data in `<external_data>...</external_data>`
            // which would otherwise trigger false positives.
            let is_xml_tag = tok.starts_with('<');
            if is_xml_tag {
                return false;
            }
            tok.contains('/') || {
                let lower = tok.to_lowercase();
                lower.ends_with(".rs")
                    || lower.ends_with(".ts")
                    || lower.ends_with(".tsx")
                    || lower.ends_with(".go")
                    || lower.ends_with(".py")
                    || lower.ends_with(".toml")
                    || lower.ends_with(".json")
            }
        })
        .count();
    file_path_count >= 3
}

pub(crate) fn is_non_decomposable_prompt_source(source: Option<&str>) -> bool {
    matches!(source, Some("periodic_review") | Some("sprint_planner"))
}

/// Check if an error message indicates a transient failure that may succeed on retry.
pub(crate) fn is_transient_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    TRANSIENT_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_error_detection() {
        // Positive cases — should match transient patterns.
        assert!(is_transient_error(
            "agent execution failed: claude exited with exit status: 1: Selected model is at capacity"
        ));
        assert!(is_transient_error("rate limit exceeded, retry after 30s"));
        assert!(is_transient_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_error("502 Bad Gateway"));
        assert!(is_transient_error(
            "Agent stream stalled: no output for 300s"
        ));
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error(
            "stream idle timeout after 300s: zombie connection terminated"
        ));
        assert!(is_transient_error(
            "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
        ));

        // Negative cases — permanent errors should not match.
        assert!(!is_transient_error(
            "Task did not receive LGTM after 5 review rounds."
        ));
        assert!(!is_transient_error(
            "triage output unparseable — agent did not produce TRIAGE=<decision>"
        ));
        assert!(!is_transient_error("all parallel subtasks failed"));
        assert!(!is_transient_error(
            "task failed unexpectedly: task 102 panicked"
        ));
        assert!(!is_transient_error(
            "budget exceeded: spent $5.00, limit $3.00"
        ));
    }

    #[test]
    fn short_prompt_does_not_require_plan() {
        let prompt = "Fix the typo in README.md";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn long_prompt_over_200_words_requires_plan() {
        let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
        let prompt = words.join(" ");
        assert!(prompt_requires_plan(&prompt));
    }

    #[test]
    fn prompt_with_three_file_paths_requires_plan() {
        let prompt =
            "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
        assert!(prompt_requires_plan(prompt));
    }

    #[test]
    fn prompt_with_two_file_paths_does_not_require_plan() {
        let prompt = "Update src/foo.rs and crates/bar/src/lib.rs to fix X";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn xml_closing_tags_are_not_counted_as_file_paths() {
        // `wrap_external_data` wraps content in <external_data>...</external_data>.
        // Three `</external_data>` closing tags contain '/' but must NOT trigger
        // the planning gate — they are markup, not file paths.
        let prompt = "GC applied files:\n\
                      <external_data>test-guard.sh</external_data>\n\
                      Rationale:\n<external_data>test</external_data>\n\
                      Validation:\n<external_data>test</external_data>";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn non_decomposable_source_list_includes_periodic_and_sprint() {
        assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
        assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
        assert!(!is_non_decomposable_prompt_source(Some("github")));
        assert!(!is_non_decomposable_prompt_source(None));
    }
}
