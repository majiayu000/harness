use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentResponse, StreamItem};
use harness_core::error::HarnessError;
use harness_core::types::{Capability, EventFilters, TokenUsage};

struct PromptPlanningAgent;

struct FailingPromptPlanningAgent;

#[async_trait]
impl CodeAgent for PromptPlanningAgent {
    fn name(&self) -> &str {
        "prompt-planner"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: "step 1\nstep 2".to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        tx.send(StreamItem::ItemCompleted {
            item: harness_core::types::Item::AgentReasoning {
                content: "step 1\nstep 2".to_string(),
            },
        })
        .await
        .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        tx.send(StreamItem::Done)
            .await
            .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl CodeAgent for FailingPromptPlanningAgent {
    fn name(&self) -> &str {
        "prompt-planner"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Err(HarnessError::AgentExecution(
            "planner echoed secret prompt".to_string(),
        ))
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        Err(HarnessError::AgentExecution(
            "planner exited with exit status: 1: stdout_tail=[secret prompt]".to_string(),
        ))
    }
}

#[tokio::test]
async fn run_plan_for_prompt_keeps_prompt_only_plan_text_out_of_rounds_and_events() {
    if std::env::var("DATABASE_URL").is_err() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db"))
        .await
        .expect("task store");
    let events = EventStore::new(dir.path()).await.expect("event store");
    let task_id = crate::task_runner::TaskId::new();
    let mut task = crate::task_runner::TaskState::new(task_id.clone());
    task.description = Some("prompt task".to_string());
    store.insert(&task).await;

    let req = CreateTaskRequest {
        prompt: Some("secret prompt".to_string()),
        ..Default::default()
    };

    let (plan, complexity, turns) = run_plan_for_prompt(
        &PromptPlanningAgent,
        &store,
        &task_id,
        &events,
        &HashMap::new(),
        dir.path(),
        &req,
    )
    .await
    .expect("prompt plan should succeed");

    assert_eq!(plan.as_deref(), Some("step 1\nstep 2"));
    assert_eq!(complexity, prompts::TriageComplexity::Medium);
    assert_eq!(turns, 1);

    let task = store.get(&task_id).expect("task state");
    assert_eq!(task.rounds.len(), 1);
    assert_eq!(task.rounds[0].action, "plan");
    assert_eq!(task.rounds[0].detail, None);

    let events = events
        .query(&EventFilters {
            hook: Some("task_plan".to_string()),
            ..Default::default()
        })
        .await
        .expect("query events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].content, None);
    assert_eq!(
        events[0].reason.as_deref(),
        Some("prompt task plan completed")
    );
}

#[tokio::test]
async fn run_plan_for_prompt_redacts_failure_metadata_before_persisting() {
    if std::env::var("DATABASE_URL").is_err() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db"))
        .await
        .expect("task store");
    let events = EventStore::new(dir.path()).await.expect("event store");
    let task_id = crate::task_runner::TaskId::new();
    let mut task = crate::task_runner::TaskState::new(task_id.clone());
    task.description = Some("prompt task".to_string());
    store.insert(&task).await;

    let req = CreateTaskRequest {
        prompt: Some("secret prompt".to_string()),
        ..Default::default()
    };

    let err = run_plan_for_prompt(
        &FailingPromptPlanningAgent,
        &store,
        &task_id,
        &events,
        &HashMap::new(),
        dir.path(),
        &req,
    )
    .await
    .expect_err("prompt plan should fail");
    assert!(
        err.to_string().contains("plan phase agent error"),
        "unexpected error: {err}"
    );
    assert!(
        !err.to_string().contains("secret prompt"),
        "returned error must not leak prompt-derived content: {err}"
    );
    assert!(
        err.to_string().contains("kind=local_process"),
        "returned error should preserve failure classification: {err}"
    );

    let task = store.get(&task_id).expect("task state");
    assert_eq!(task.rounds.len(), 1);
    let failure = task.rounds[0].failure.as_ref().expect("round failure");
    assert!(
        failure.message.is_none(),
        "failure message should be redacted"
    );
    assert!(
        failure.body_excerpt.is_none(),
        "failure body excerpt should be redacted"
    );

    let events = events
        .query(&EventFilters {
            hook: Some("task_plan".to_string()),
            ..Default::default()
        })
        .await
        .expect("query events");
    assert_eq!(events.len(), 1);
    let failure = events[0]
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.failure.as_ref())
        .expect("event failure metadata");
    assert!(
        failure.message.is_none(),
        "event failure message should be redacted"
    );
    assert!(
        failure.body_excerpt.is_none(),
        "event failure body excerpt should be redacted"
    );
}

#[test]
fn redact_prompt_plan_failure_clears_message_and_excerpt() {
    let failure = TurnFailure {
        kind: harness_core::types::TurnFailureKind::Unknown,
        provider: Some("codex".to_string()),
        upstream_status: Some(1),
        message: Some("secret prompt".to_string()),
        body_excerpt: Some("still secret".to_string()),
    };

    let redacted = redact_prompt_plan_failure(failure);
    assert_eq!(redacted.provider.as_deref(), Some("codex"));
    assert_eq!(redacted.upstream_status, Some(1));
    assert!(redacted.message.is_none());
    assert!(redacted.body_excerpt.is_none());
}

#[test]
fn redact_prompt_plan_error_message_preserves_only_safe_fields() {
    let message = redact_prompt_plan_error_message(&TurnFailure {
        kind: TurnFailureKind::Upstream,
        provider: Some("anthropic-api".to_string()),
        upstream_status: Some(500),
        message: Some("secret prompt".to_string()),
        body_excerpt: Some("still secret".to_string()),
    });

    assert_eq!(
        message,
        "plan phase agent error (details redacted for prompt-only task privacy; kind=upstream, provider=anthropic-api, upstream_status=500)"
    );
    assert!(!message.contains("secret prompt"));
    assert!(!message.contains("still secret"));
}
