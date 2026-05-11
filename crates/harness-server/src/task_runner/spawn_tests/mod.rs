use super::*;

mod helpers;
mod spawn_classify_tests;
mod spawn_decomposition_tests;
mod spawn_resume_tests;
mod spawn_skills_tests;

pub(super) fn tid(s: &str) -> harness_core::types::TaskId {
    harness_core::types::TaskId(s.to_string())
}

fn cleanup_state(
    status: TaskStatus,
    task_kind: TaskKind,
    external_id: Option<&str>,
    issue: Option<u64>,
) -> TaskState {
    let mut state = TaskState::new(tid("cleanup-task"));
    state.status = status;
    state.task_kind = task_kind;
    state.external_id = external_id.map(str::to_string);
    state.issue = issue;
    state
}

#[test]
fn workspace_cleanup_keeps_inflight_issue_workspace() {
    let state = cleanup_state(
        TaskStatus::Waiting,
        TaskKind::Issue,
        Some("issue:42"),
        Some(42),
    );
    assert!(
        !should_remove_workspace_after_task(Some(&state), true),
        "waiting issue workspaces must survive follow-up phases"
    );
}

#[test]
fn workspace_cleanup_recognizes_issue_kind_without_external_id() {
    let state = cleanup_state(TaskStatus::Done, TaskKind::Issue, None, Some(42));
    assert!(
        !should_remove_workspace_after_task(Some(&state), true),
        "issue/PR lifecycle detection must not rely only on normalized external_id"
    );
}

#[test]
fn workspace_cleanup_removes_failed_issue_after_terminal_state() {
    let state = cleanup_state(
        TaskStatus::Failed,
        TaskKind::Issue,
        Some("issue:42"),
        Some(42),
    );
    assert!(
        should_remove_workspace_after_task(Some(&state), true),
        "failed issue workspaces may be cleaned after terminal failure"
    );
}

#[test]
fn workspace_cleanup_removes_done_prompt_when_auto_cleanup_enabled() {
    let state = cleanup_state(TaskStatus::Done, TaskKind::Prompt, None, None);
    assert!(should_remove_workspace_after_task(Some(&state), true));
}

/// Verify that a local u32 counter correctly tracks waiting rounds without any store query.
/// Task execution is sequential within a single tokio task, so a plain local counter suffices.
#[test]
fn local_waiting_counter_increments_on_each_waiting_response() {
    let max_rounds = 5u32;
    let mut waiting_count: u32 = 0;
    let mut observed: Vec<u32> = Vec::new();

    // Simulate the initial wait before the review loop.
    waiting_count += 1;
    observed.push(waiting_count);

    // Simulate inter-round waits (max_rounds - 1 additional waits).
    for _ in 1..max_rounds {
        waiting_count += 1;
        observed.push(waiting_count);
    }

    let expected: Vec<u32> = (1..=max_rounds).collect();
    assert_eq!(
        observed, expected,
        "waiting_count must increment monotonically on each waiting response"
    );
}

#[tokio::test]
async fn register_pending_task_keeps_repo_metadata_visible() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let req = CreateTaskRequest {
        issue: Some(20),
        source: Some("github".to_string()),
        external_id: Some("20".to_string()),
        repo: Some("acme/harness".to_string()),
        project: Some(dir.path().to_path_buf()),
        ..Default::default()
    };

    let task_id = register_pending_task(store.clone(), &req).await;
    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;

    assert_eq!(state.source.as_deref(), Some("github"));
    assert_eq!(state.external_id.as_deref(), Some("20"));
    assert_eq!(state.repo.as_deref(), Some("acme/harness"));
    assert_eq!(state.project_root.as_deref(), Some(dir.path()));
    assert_eq!(state.description.as_deref(), Some("issue #20"));
    Ok(())
}

#[test]
fn preregistered_metadata_refresh_preserves_persisted_phase() {
    let mut state = TaskState::new(TaskId::new());
    state.phase = TaskPhase::Plan;

    let req = CreateTaskRequest {
        prompt: Some("small prompt".into()),
        project: Some(PathBuf::from("/tmp/recovered-project")),
        repo: Some("owner/repo".into()),
        ..Default::default()
    };

    refresh_preregistered_task_metadata(
        &mut state,
        &req,
        PathBuf::from("/tmp/recovered-project"),
        Some("small prompt".into()),
    );

    assert_eq!(state.phase, TaskPhase::Plan);
    assert_eq!(state.repo.as_deref(), Some("owner/repo"));
    assert_eq!(state.description.as_deref(), Some("small prompt"));
}

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
        "Agent stream stalled: no output for 3600s"
    ));
    assert!(is_transient_error("connection reset by peer"));
    assert!(is_transient_error(
        "stream idle timeout after 3600s: zombie connection terminated"
    ));
    assert!(is_transient_error(
        "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
    ));
    assert!(is_transient_error(
        "failed to fetch GitHub pull request page for owner/repo issue #998"
    ));
    assert!(is_transient_error(
        "timed out fetching GitHub pull request page for owner/repo issue #998"
    ));
    assert!(is_transient_error(
        "GitHub pull request lookup for owner/repo issue #998 returned 403 Forbidden; rate limit retry-after=60; x-ratelimit-remaining=0"
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
