use super::review_loop::{record_runtime_pr_feedback, run_review_loop};
use crate::event_replay::TaskEvent;
use crate::task_runner::{
    mutate_and_persist, CreateTaskRequest, RoundResult, TaskId, TaskPhase, TaskState, TaskStatus,
    TaskStore, TaskTerminalFailure,
};
use chrono::Utc;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::types::{Capability, TokenUsage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

struct StaticReviewAgent {
    output: &'static str,
    requests: Mutex<u32>,
}

impl StaticReviewAgent {
    fn new(output: &'static str) -> Self {
        Self {
            output,
            requests: Mutex::new(0),
        }
    }

    async fn request_count(&self) -> u32 {
        *self.requests.lock().await
    }
}

#[async_trait::async_trait]
impl CodeAgent for StaticReviewAgent {
    fn name(&self) -> &str {
        "static-review-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        *self.requests.lock().await += 1;
        Ok(AgentResponse {
            output: self.output.to_string(),
            stderr: String::new(),
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: "test".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        *self.requests.lock().await += 1;
        let _ = tx
            .send(StreamItem::MessageDelta {
                text: self.output.to_string(),
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

#[tokio::test]
async fn review_loop_signal_fetch_failure_fails_before_prompt_when_wait_budget_exceeded(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let agent = StaticReviewAgent::new("LGTM");
    let review_config = harness_core::config::agents::AgentReviewConfig {
        review_wait_budget_secs: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let interceptors = Arc::new(Vec::new());
    let mut turns_used = 0;
    let mut turns_used_acc = 0;

    run_review_loop(
        store.as_ref(),
        &task_id,
        &agent,
        &review_config,
        &harness_core::config::project::ProjectConfig::default(),
        None,
        None,
        dir.path(),
        &CreateTaskRequest::default(),
        &events,
        &interceptors,
        &[],
        dir.path(),
        &HashMap::new(),
        Some("https://github.com/owner/repo/pull/1".to_string()),
        1,
        None,
        1,
        0,
        1,
        false,
        false,
        Duration::from_secs(5),
        &mut turns_used,
        &mut turns_used_acc,
        Instant::now(),
        Instant::now() - Duration::from_secs(2),
        "invalid-repo-slug".to_string(),
        0.9,
        None,
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_eq!(final_state.status, TaskStatus::Failed);
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("review wait budget exceeded"),
        "unexpected error: {:?}",
        final_state.error
    );
    assert_eq!(agent.request_count().await, 0);
    Ok(())
}

#[tokio::test]
async fn review_loop_waiting_output_fails_when_wait_budget_exceeded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let agent = StaticReviewAgent::new("Review bot has not replied yet.\nWAITING");
    let review_config = harness_core::config::agents::AgentReviewConfig {
        review_wait_budget_secs: 1,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let interceptors = Arc::new(Vec::new());
    let mut turns_used = 0;
    let mut turns_used_acc = 0;

    run_review_loop(
        store.as_ref(),
        &task_id,
        &agent,
        &review_config,
        &harness_core::config::project::ProjectConfig::default(),
        None,
        None,
        dir.path(),
        &CreateTaskRequest::default(),
        &events,
        &interceptors,
        &[],
        dir.path(),
        &HashMap::new(),
        Some("https://github.com/owner/repo/pull/1".to_string()),
        1,
        None,
        1,
        0,
        1,
        false,
        false,
        Duration::from_secs(5),
        &mut turns_used,
        &mut turns_used_acc,
        Instant::now(),
        Instant::now() - Duration::from_secs(2),
        "invalid-repo-slug".to_string(),
        0.9,
        None,
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_eq!(final_state.status, TaskStatus::Failed);
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("review wait budget exceeded"),
        "unexpected error: {:?}",
        final_state.error
    );
    Ok(())
}

#[tokio::test]
async fn review_loop_round_budget_exhausted_marks_terminal_once() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store.insert(&TaskState::new(task_id.clone())).await;
    let agent =
        StaticReviewAgent::new("Found 4 issues.\n1. Missing test\n2. Race\n3. Leak\n4. Panic");
    let review_config = harness_core::config::agents::AgentReviewConfig {
        review_wait_budget_secs: 60,
        ..harness_core::config::agents::AgentReviewConfig::default()
    };
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let interceptors = Arc::new(Vec::new());
    let mut turns_used = 0;
    let mut turns_used_acc = 0;

    run_review_loop(
        store.as_ref(),
        &task_id,
        &agent,
        &review_config,
        &harness_core::config::project::ProjectConfig::default(),
        None,
        None,
        dir.path(),
        &CreateTaskRequest::default(),
        &events,
        &interceptors,
        &[],
        dir.path(),
        &HashMap::new(),
        Some("https://github.com/owner/repo/pull/1".to_string()),
        1,
        None,
        1,
        0,
        1,
        false,
        false,
        Duration::from_secs(5),
        &mut turns_used,
        &mut turns_used_acc,
        Instant::now(),
        Instant::now(),
        "invalid-repo-slug".to_string(),
        0.9,
        None,
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task state should exist"))?;
    assert_eq!(final_state.status, TaskStatus::Failed);
    assert_eq!(final_state.phase, TaskPhase::Terminal);
    assert_eq!(final_state.turn, 2);
    let failure: TaskTerminalFailure = serde_json::from_str(
        final_state
            .error
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("terminal failure should include a reason"))?,
    )?;
    assert_eq!(failure.reason, "round_budget_exhausted");
    assert_eq!(failure.rounds_used, 1);
    assert_eq!(failure.last_status, TaskStatus::Reviewing);
    assert_eq!(failure.waiting_on.as_deref(), Some("local_review_gate"));
    assert_eq!(agent.request_count().await, 1);

    let event_log = std::fs::read_to_string(dir.path().join("task-events.jsonl"))?;
    let failed_events = event_log
        .lines()
        .filter_map(|line| serde_json::from_str::<TaskEvent>(line).ok())
        .filter(|event| {
            matches!(
                event,
                TaskEvent::Failed { reason, .. }
                    if reason.contains(r#""reason":"round_budget_exhausted""#)
            )
        })
        .count();
    assert_eq!(failed_events, 1);
    Ok(())
}

struct TierCTestStores {
    tasks: Arc<TaskStore>,
    issue_workflows: harness_workflow::issue_lifecycle::IssueWorkflowStore,
    runtime: harness_workflow::runtime::WorkflowRuntimeStore,
    database_url: String,
    task_path: std::path::PathBuf,
    project_root: std::path::PathBuf,
}

async fn required_tier_c_stores(dir: &std::path::Path) -> anyhow::Result<TierCTestStores> {
    let configured = std::env::var("HARNESS_DATABASE_URL").map_err(|_| {
        anyhow::anyhow!(
            "GH1715 Tier-C tests require HARNESS_DATABASE_URL for an isolated disposable database"
        )
    })?;
    let database_url = harness_core::db::resolve_test_database_url(Some(&configured))?;
    let task_path = dir.join("gh1715-tasks.db");
    let project_root = dir.join("project");
    std::fs::create_dir_all(&project_root)?;
    Ok(TierCTestStores {
        tasks: TaskStore::open_with_database_url(&task_path, Some(&database_url)).await?,
        issue_workflows:
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
                &dir.join("gh1715-issue-workflows.db"),
                Some(&database_url),
            )
            .await?,
        runtime: harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.join("gh1715-runtime"),
            Some(&database_url),
        )
        .await?,
        database_url,
        task_path,
        project_root,
    })
}

async fn seed_tier_c_workflows(
    stores: &TierCTestStores,
    task_id: &TaskId,
    issue: u64,
    pr: u64,
) -> anyhow::Result<()> {
    stores.tasks.insert(&TaskState::new(task_id.clone())).await;
    let project = stores.project_root.to_string_lossy();
    stores
        .issue_workflows
        .record_issue_scheduled(
            project.as_ref(),
            Some("owner/repo"),
            issue,
            task_id.as_str(),
            &[],
            false,
        )
        .await?;
    stores
        .issue_workflows
        .record_pr_detected(
            project.as_ref(),
            Some("owner/repo"),
            issue,
            task_id.as_str(),
            pr,
            &format!("https://github.com/owner/repo/pull/{pr}"),
        )
        .await?;
    crate::workflow_runtime_pr_feedback::record_pr_detected(
        Some(&stores.runtime),
        crate::workflow_runtime_pr_feedback::PrDetectedRuntimeContext {
            project_root: &stores.project_root,
            repo: Some("owner/repo"),
            issue_number: issue,
            task_id,
            pr_number: pr,
            pr_url: &format!("https://github.com/owner/repo/pull/{pr}"),
        },
    )
    .await;
    Ok(())
}

fn tier_c_snapshot(
    activated_at: chrono::DateTime<chrono::Utc>,
) -> harness_workflow::issue_lifecycle::ReviewFallbackSnapshot {
    harness_workflow::issue_lifecycle::ReviewFallbackSnapshot {
        tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
        trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence,
        active_bot: Some("codex".to_string()),
        activated_at,
    }
}

fn completed_event_count(path: &std::path::Path, task_id: &TaskId) -> anyhow::Result<usize> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    Ok(content
        .lines()
        .filter_map(|line| serde_json::from_str::<TaskEvent>(line).ok())
        .filter(|event| {
            matches!(
                event,
                TaskEvent::Completed { task_id: completed, .. } if completed == task_id.as_str()
            )
        })
        .count())
}

#[tokio::test]
#[ignore = "requires isolated HARNESS_DATABASE_URL"]
async fn tier_c_lifecycle_rejection_records_no_completion_evidence() -> anyhow::Result<()> {
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let stores = required_tier_c_stores(dir.path()).await?;
    let task_id = TaskId::from_str("gh1715-tier-c-rejection");
    seed_tier_c_workflows(&stores, &task_id, 1715, 2715).await?;
    stores
        .issue_workflows
        .record_pr_merged(
            &stores.project_root.to_string_lossy(),
            Some("owner/repo"),
            2715,
            None,
        )
        .await?;
    let error = stores
        .issue_workflows
        .record_ready_to_merge_with_fallback(
            &stores.project_root.to_string_lossy(),
            Some("owner/repo"),
            2715,
            Some("Tier C"),
            tier_c_snapshot(Utc::now()),
        )
        .await
        .expect_err("terminal lifecycle must reject Tier-C readiness");
    assert!(error.to_string().contains("transition_not_allowed"));
    let task = stores.tasks.get(&task_id).expect("task");
    assert_ne!(task.status, TaskStatus::Done);
    assert!(task.rounds.is_empty());
    assert_eq!(
        completed_event_count(&dir.path().join("task-events.jsonl"), &task_id)?,
        0
    );
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &stores.project_root.to_string_lossy(),
        Some("owner/repo"),
        1715,
    );
    assert!(stores
        .runtime
        .events_for(&workflow_id)
        .await?
        .iter()
        .all(|event| event.event_type != "PrReadyToMerge"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires isolated HARNESS_DATABASE_URL"]
async fn tier_c_task_store_failure_retry_records_completion_once() -> anyhow::Result<()> {
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let stores = required_tier_c_stores(dir.path()).await?;
    let task_id = TaskId::from_str("gh1715-tier-c-retry");
    seed_tier_c_workflows(&stores, &task_id, 1716, 2716).await?;
    let first_activated_at = Utc::now();
    stores
        .issue_workflows
        .record_ready_to_merge_with_fallback(
            &stores.project_root.to_string_lossy(),
            Some("owner/repo"),
            2716,
            Some("Tier C"),
            tier_c_snapshot(first_activated_at),
        )
        .await?;

    let conflicting_store =
        TaskStore::open_with_database_url(&stores.task_path, Some(&stores.database_url)).await?;
    mutate_and_persist(&conflicting_store, &task_id, |task| {
        task.error = Some("concurrent update".to_string());
    })
    .await?;
    mutate_and_persist(&stores.tasks, &task_id, |task| {
        task.status = TaskStatus::Done;
    })
    .await
    .expect_err("stale task version must force the post-lifecycle mutation to fail");
    assert_ne!(
        stores.tasks.get(&task_id).expect("task").status,
        TaskStatus::Done
    );

    let retry_store =
        TaskStore::open_with_database_url(&stores.task_path, Some(&stores.database_url)).await?;
    let retried = stores
        .issue_workflows
        .record_ready_to_merge_with_fallback(
            &stores.project_root.to_string_lossy(),
            Some("owner/repo"),
            2716,
            Some("Tier C"),
            tier_c_snapshot(Utc::now()),
        )
        .await?
        .expect("workflow");
    assert_eq!(
        retried.review_fallback.expect("fallback").activated_at,
        first_activated_at
    );
    mutate_and_persist(&retry_store, &task_id, |task| {
        task.status = TaskStatus::Done;
        task.turn = 1;
        task.rounds.push(RoundResult::new(
            1,
            "review",
            "ready_to_merge",
            Some("Tier C".to_string()),
            None,
            None,
        ));
    })
    .await?;
    let request = CreateTaskRequest {
        issue: Some(1716),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };
    record_runtime_pr_feedback(
        Some(&stores.issue_workflows),
        Some(&stores.runtime),
        &stores.project_root,
        &request,
        &task_id,
        2716,
        Some("https://github.com/owner/repo/pull/2716"),
        harness_workflow::runtime::PrFeedbackOutcome::ReadyToMerge,
        "Tier C",
    )
    .await;
    retry_store.log_event(TaskEvent::Completed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
    });
    let task = retry_store.get(&task_id).expect("task");
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(
        task.rounds
            .iter()
            .filter(|round| round.result == "ready_to_merge")
            .count(),
        1
    );
    assert_eq!(
        completed_event_count(&dir.path().join("task-events.jsonl"), &task_id)?,
        1
    );
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &stores.project_root.to_string_lossy(),
        Some("owner/repo"),
        1716,
    );
    assert_eq!(
        stores
            .runtime
            .events_for(&workflow_id)
            .await?
            .iter()
            .filter(|event| event.event_type == "PrReadyToMerge")
            .count(),
        1
    );
    Ok(())
}
