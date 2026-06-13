use super::*;
use tokio::time::Duration;

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

fn agent_response(output: &str) -> harness_core::agent::AgentResponse {
    harness_core::agent::AgentResponse {
        output: output.to_string(),
        stderr: String::new(),
        items: vec![],
        token_usage: harness_core::types::TokenUsage::default(),
        model: "mock".into(),
        exit_code: Some(0),
    }
}

#[test]
fn subtask_empty_output_marks_parent_failed() {
    let mut state = TaskState::new(tid("parallel-parent"));
    let run_result = crate::parallel_dispatch::ParallelRunResult {
        results: vec![
            crate::parallel_dispatch::SubtaskResult {
                index: 0,
                response: Some(agent_response("")),
                error: None,
            },
            crate::parallel_dispatch::SubtaskResult {
                index: 1,
                response: Some(agent_response("finished work")),
                error: None,
            },
        ],
        is_sequential: false,
    };

    record_parallel_subtask_results(&mut state, &run_result);

    assert_eq!(state.status, TaskStatus::Failed);
    assert_eq!(state.error.as_deref(), Some("1/2 parallel subtasks failed"));
    assert_eq!(state.rounds.len(), 2);
    assert_eq!(state.rounds[0].result, "failed");
    assert_eq!(
        state.rounds[0].detail.as_deref(),
        Some("agent returned empty output")
    );
    assert_eq!(state.rounds[1].result, "success");
    assert_eq!(state.rounds[1].detail.as_deref(), Some("finished work"));
}

#[test]
fn sequential_subtask_whitespace_output_marks_parent_failed() {
    let mut state = TaskState::new(tid("sequential-parent"));
    let run_result = crate::parallel_dispatch::ParallelRunResult {
        results: vec![crate::parallel_dispatch::SubtaskResult {
            index: 0,
            response: Some(agent_response(" \n\t")),
            error: None,
        }],
        is_sequential: true,
    };

    record_parallel_subtask_results(&mut state, &run_result);

    assert_eq!(state.status, TaskStatus::Failed);
    assert_eq!(
        state.error.as_deref(),
        Some("1/1 sequential subtasks failed; remaining steps were skipped")
    );
    assert_eq!(state.rounds.len(), 1);
    assert_eq!(state.rounds[0].result, "failed");
    assert_eq!(
        state.rounds[0].detail.as_deref(),
        Some("agent returned empty output")
    );
}

#[test]
fn subtask_non_empty_outputs_mark_parent_done() {
    let mut state = TaskState::new(tid("parallel-parent"));
    let run_result = crate::parallel_dispatch::ParallelRunResult {
        results: vec![
            crate::parallel_dispatch::SubtaskResult {
                index: 0,
                response: Some(agent_response("first result")),
                error: None,
            },
            crate::parallel_dispatch::SubtaskResult {
                index: 1,
                response: Some(agent_response("second result")),
                error: None,
            },
        ],
        is_sequential: false,
    };

    record_parallel_subtask_results(&mut state, &run_result);

    assert_eq!(state.status, TaskStatus::Done);
    assert_eq!(state.error, None);
    assert_eq!(state.rounds.len(), 2);
    assert!(state
        .rounds
        .iter()
        .all(|round| round.result == "success" && round.detail.is_some()));
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

#[test]
fn abort_after_handle_registration_only_targets_live_terminal_tasks() {
    let mut cancelled = TaskState::new(tid("cancelled"));
    cancelled.status = TaskStatus::Cancelled;
    assert!(
        should_abort_after_abort_handle_registration(&cancelled, false),
        "a terminal task with a live handle must be aborted"
    );
    assert!(
        !should_abort_after_abort_handle_registration(&cancelled, true),
        "a task that already finished itself must not be aborted again"
    );

    let mut implementing = TaskState::new(tid("implementing"));
    implementing.status = TaskStatus::Implementing;
    assert!(
        !should_abort_after_abort_handle_registration(&implementing, false),
        "non-terminal tasks must keep running"
    );
}

#[tokio::test]
async fn cancelled_abort_releases_workspace_after_watcher_observes_join() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let project_root = dir.path().join("project");
    init_spawn_test_repo(&project_root)?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await?,
    );
    let agent = helpers::BlockingAgent::new();
    let mut workspace_config = harness_core::config::misc::WorkspaceConfig {
        root: dir.path().join("workspaces"),
        ..Default::default()
    };
    workspace_config.root_configured = true;
    let workspace_mgr = Arc::new(crate::workspace::WorkspaceManager::new(workspace_config)?);
    let req = CreateTaskRequest {
        project: Some(project_root.clone()),
        prompt: Some("block until cancelled".to_string()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 3600,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    let task_id = spawn_task(
        store.clone(),
        agent.clone(),
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        Some(workspace_mgr.clone()),
        permit,
        None,
        None,
        None,
        vec![],
    )
    .await;

    agent.wait_started(Duration::from_secs(15)).await?;
    assert_eq!(workspace_mgr.live_count(), 1);
    let workspace_path = store
        .get(&task_id)
        .and_then(|state| state.workspace_path)
        .expect("workspace path should be persisted after admission");
    assert!(
        workspace_path.exists(),
        "workspace should exist before cancellation"
    );
    mutate_and_persist(&store, &task_id, |s| {
        s.status = TaskStatus::Cancelled;
        s.scheduler.mark_terminal(&TaskStatus::Cancelled);
    })
    .await?;
    assert_eq!(
        workspace_mgr.live_count(),
        1,
        "cancelling the row must not release the workspace before abort is observed"
    );

    assert!(
        store.abort_task(&task_id),
        "running task should have an abort handle"
    );
    helpers::wait_until(Duration::from_secs(15), || workspace_mgr.live_count() == 0).await?;
    assert!(
        !workspace_path.exists(),
        "cancelled prompt workspace should be removed after abort teardown"
    );
    assert_eq!(
        store.get(&task_id).map(|state| state.status),
        Some(TaskStatus::Cancelled)
    );
    Ok(())
}

fn init_spawn_test_repo(root: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root)?;
    run_spawn_test_git(root, &["init"])?;
    run_spawn_test_git(root, &["config", "user.email", "test@harness.test"])?;
    run_spawn_test_git(root, &["config", "user.name", "Harness Test"])?;
    run_spawn_test_git(root, &["commit", "--allow-empty", "-m", "init"])?;
    run_spawn_test_git(root, &["branch", "-M", "main"])?;
    run_spawn_test_git(root, &["remote", "add", "origin", &root.to_string_lossy()])?;
    Ok(())
}

fn run_spawn_test_git(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let git_bin = std::env::var("HARNESS_GIT_BIN").unwrap_or_else(|_| "git".to_string());
    let mut command = std::process::Command::new(git_bin);
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(key);
    }
    let output = command.arg("-C").arg(root).args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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
