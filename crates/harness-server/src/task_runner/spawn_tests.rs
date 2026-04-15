use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
use harness_core::types::{Capability, ExecutionPhase, TokenUsage};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Duration;

#[tokio::test]
async fn register_pending_task_keeps_repo_metadata_visible() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let req = CreateTaskRequest {
        issue: Some(20),
        source: Some("github".to_string()),
        external_id: Some("20".to_string()),
        repo: Some("acme/harness".to_string()),
        ..Default::default()
    };

    let task_id = register_pending_task(store.clone(), &req).await;
    let state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task not found in store — possible concurrent deletion"))?;

    assert_eq!(state.source.as_deref(), Some("github"));
    assert_eq!(state.external_id.as_deref(), Some("20"));
    assert_eq!(state.repo.as_deref(), Some("acme/harness"));
    assert_eq!(state.description.as_deref(), Some("issue #20"));
    Ok(())
}

/// Mock agent that records the `execution_phase` from every call and
/// returns pre-configured responses in order.
struct PhaseCapturingAgent {
    phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
    responses: tokio::sync::Mutex<Vec<String>>,
}

impl PhaseCapturingAgent {
    fn new(responses: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            phases: tokio::sync::Mutex::new(Vec::new()),
            responses: tokio::sync::Mutex::new(responses),
        })
    }

    async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
        self.phases.lock().await.clone()
    }

    async fn next_response(&self) -> String {
        let mut guard = self.responses.lock().await;
        if guard.is_empty() {
            String::new()
        } else {
            guard.remove(0)
        }
    }
}

#[async_trait]
impl harness_core::agent::CodeAgent for PhaseCapturingAgent {
    fn name(&self) -> &str {
        "phase-capturing-mock"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.phases.lock().await.push(req.execution_phase);
        let output = self.next_response().await;
        Ok(AgentResponse {
            output,
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".into(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.phases.lock().await.push(req.execution_phase);
        let output = self.next_response().await;
        if !output.is_empty() {
            if let Err(e) = tx.send(StreamItem::MessageDelta { text: output }).await {
                tracing::warn!("PhaseCapturingAgent: failed to send MessageDelta: {e}");
            }
        }
        if let Err(e) = tx.send(StreamItem::Done).await {
            tracing::warn!("PhaseCapturingAgent: failed to send Done: {e}");
        }
        Ok(())
    }
}

#[tokio::test]
async fn planning_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    // Agent returns empty output (no PR URL) → task completes after implementation.
    let agent = PhaseCapturingAgent::new(vec![String::new()]);
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("implement something".into()),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(0),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let phases = agent.captured_phases().await;
    assert!(
        !phases.is_empty(),
        "expected at least one agent call, got none"
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Planning),
        "initial implementation turn must use Planning phase"
    );
    Ok(())
}

#[tokio::test]
async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

    // Call 1 (execute_stream): return a PR URL to trigger the review loop.
    // Call 2 (execute): return LGTM to complete the review loop.
    let agent = PhaseCapturingAgent::new(vec![
        "PR_URL=https://github.com/owner/repo/pull/1".into(),
        "LGTM".into(),
    ]);
    let agent_clone = agent.clone();

    let req = CreateTaskRequest {
        prompt: Some("implement something".into()),
        project: Some(dir.path().to_path_buf()),
        wait_secs: 0,
        max_rounds: Some(1),
        turn_timeout_secs: 30,
        ..Default::default()
    };

    let queue = crate::task_queue::TaskQueue::unbounded();
    let permit = queue.acquire("test", 0).await?;
    spawn_task(
        store,
        agent_clone,
        None,
        Default::default(),
        skills,
        events,
        vec![],
        req,
        None,
        permit,
        None,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let phases = agent.captured_phases().await;
    assert!(
        phases.len() >= 2,
        "expected at least 2 agent calls (implementation + review check), got {}",
        phases.len()
    );
    assert_eq!(
        phases[0],
        Some(ExecutionPhase::Planning),
        "implementation turn must use Planning phase"
    );
    assert_eq!(
        phases[1],
        Some(ExecutionPhase::Execution),
        "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
    );
    Ok(())
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
fn parse_pr_url_standard() {
    let Some((owner, repo, number)) =
        scheduling::parse_pr_url("https://github.com/acme/myrepo/pull/42")
    else {
        panic!("expected Some for standard GitHub PR URL");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 42);
}

#[test]
fn parse_pr_url_with_fragment() {
    let Some((owner, repo, number)) =
        scheduling::parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
    else {
        panic!("expected Some for PR URL with fragment");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 99);
}

#[test]
fn parse_pr_url_trailing_slash() {
    let Some((owner, repo, number)) =
        scheduling::parse_pr_url("https://github.com/acme/myrepo/pull/7/")
    else {
        panic!("expected Some for PR URL with trailing slash");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 7);
}

#[test]
fn parse_pr_url_invalid_returns_none() {
    assert!(scheduling::parse_pr_url("https://github.com/acme/myrepo").is_none());
    assert!(scheduling::parse_pr_url("not-a-url").is_none());
    assert!(scheduling::parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
}

// --- planning gate ---

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
    let prompt = "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
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

#[test]
fn task_status_semantics_are_centralized() {
    let cases = [
        (
            TaskStatus::Pending,
            false,
            false,
            false,
            false,
            false,
            false,
        ),
        (
            TaskStatus::AwaitingDeps,
            false,
            false,
            false,
            false,
            false,
            false,
        ),
        (
            TaskStatus::Implementing,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (
            TaskStatus::AgentReview,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (TaskStatus::Waiting, false, true, true, false, false, false),
        (
            TaskStatus::Reviewing,
            false,
            true,
            true,
            false,
            false,
            false,
        ),
        (TaskStatus::Done, true, false, false, true, false, false),
        (TaskStatus::Failed, true, false, false, false, true, false),
        (
            TaskStatus::Cancelled,
            true,
            false,
            false,
            false,
            false,
            true,
        ),
    ];

    for (status, terminal, inflight, resumable, success, failure, cancelled) in cases {
        assert_eq!(status.is_terminal(), terminal, "{status:?} terminal");
        assert_eq!(status.is_inflight(), inflight, "{status:?} inflight");
        assert_eq!(
            status.is_resumable_after_restart(),
            resumable,
            "{status:?} resumable"
        );
        assert_eq!(status.is_success(), success, "{status:?} success");
        assert_eq!(status.is_failure(), failure, "{status:?} failure");
        assert_eq!(status.is_cancelled(), cancelled, "{status:?} cancelled");
    }

    assert_eq!(
        TaskStatus::terminal_statuses(),
        &["done", "failed", "cancelled"]
    );
    assert_eq!(
        TaskStatus::resumable_statuses(),
        &["implementing", "agent_review", "waiting", "reviewing"]
    );
}

#[tokio::test]
async fn count_by_project_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    assert!(store.count_for_dashboard().await.by_project.is_empty());
    Ok(())
}

#[tokio::test]
async fn count_by_project_none_root_excluded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let mut task = TaskState::new(harness_core::types::TaskId("no-root".to_string()));
    task.status = TaskStatus::Done;
    // project_root stays None
    store.insert(&task).await;

    assert!(
        store.count_for_dashboard().await.by_project.is_empty(),
        "tasks with no project_root must not appear in per-project counts"
    );
    Ok(())
}

#[tokio::test]
async fn count_by_project_groups_correctly() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let root_a = std::path::PathBuf::from("/projects/alpha");
    let root_b = std::path::PathBuf::from("/projects/beta");

    for (id, root, status) in [
        ("a1", &root_a, TaskStatus::Done),
        ("a2", &root_a, TaskStatus::Done),
        ("a3", &root_a, TaskStatus::Failed),
        ("a4", &root_a, TaskStatus::Cancelled),
        ("b1", &root_b, TaskStatus::Done),
        ("b2", &root_b, TaskStatus::Failed),
        ("b3", &root_b, TaskStatus::Failed),
        ("b4", &root_b, TaskStatus::Cancelled),
    ] {
        let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
        task.status = status;
        task.project_root = Some(root.clone());
        store.insert(&task).await;
    }

    let counts = store.count_for_dashboard().await.by_project;
    let key_a = root_a.to_string_lossy().into_owned();
    let key_b = root_b.to_string_lossy().into_owned();

    assert!(counts.contains_key(&key_a), "alpha counts missing");
    assert_eq!(counts[&key_a].done, 2, "alpha done");
    assert_eq!(counts[&key_a].failed, 1, "alpha failed");

    assert!(counts.contains_key(&key_b), "beta counts missing");
    assert_eq!(counts[&key_b].done, 1, "beta done");
    assert_eq!(counts[&key_b].failed, 2, "beta failed");
    Ok(())
}

#[tokio::test]
async fn count_by_project_excludes_cancelled_from_failed_totals() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

    let root = std::path::PathBuf::from("/projects/alpha");
    for (id, status) in [
        ("done", TaskStatus::Done),
        ("failed", TaskStatus::Failed),
        ("cancelled", TaskStatus::Cancelled),
    ] {
        let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
        task.status = status;
        task.project_root = Some(root.clone());
        store.insert(&task).await;
    }

    let counts = store.count_for_dashboard().await;
    assert_eq!(counts.global_done, 1);
    assert_eq!(counts.global_failed, 1);
    let key = root.to_string_lossy().into_owned();
    assert_eq!(counts.by_project[&key].done, 1);
    assert_eq!(counts.by_project[&key].failed, 1);
    Ok(())
}
