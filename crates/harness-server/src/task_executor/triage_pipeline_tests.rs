use super::*;
use async_trait::async_trait;
use harness_core::agent::{AgentResponse, StreamItem};
use harness_core::error::HarnessError;
use harness_core::types::{Capability, EventFilters, TokenUsage};
use std::sync::Arc;

struct PromptPlanningAgent;

struct FailingPromptPlanningAgent;

struct TriageStaticStreamAgent {
    output: String,
}

impl TriageStaticStreamAgent {
    fn new(output: &str) -> Self {
        Self {
            output: output.to_string(),
        }
    }
}

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

#[async_trait]
impl CodeAgent for TriageStaticStreamAgent {
    fn name(&self) -> &str {
        "triage-static-stream-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: self.output.clone(),
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
        tx.send(StreamItem::MessageDelta {
            text: self.output.clone(),
        })
        .await
        .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        tx.send(StreamItem::Done)
            .await
            .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        Ok(())
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
    let skills = RwLock::new(harness_skills::store::SkillStore::new());

    let (plan, complexity, turns) = run_plan_for_prompt(
        &PromptPlanningAgent,
        &store,
        &task_id,
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
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
    let skills = RwLock::new(harness_skills::store::SkillStore::new());

    let err = run_plan_for_prompt(
        &FailingPromptPlanningAgent,
        &store,
        &task_id,
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
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

fn actionable_issue_body() -> String {
    r#"
packages/core/src/infra/sqlite/ops.rs:72

## Recommended Fix
1. Bind parameters instead of concatenating raw input.
2. Add a regression test for repeated submissions.

## Acceptance Criteria
- Reject SQL metacharacters in the failing path.
- Preserve current successful requests.

```rust
let statement = conn.prepare_cached(SQL)?;
```
"#
    .to_string()
}

fn abstract_issue_body() -> String {
    "We should think about this area again later.\nNo concrete path or implementation detail yet."
        .to_string()
}

fn actionable_review_issue_fetcher<'a>(
    _repo_slug: &'a str,
    _issue: u64,
    _github_token: Option<&'a str>,
) -> IssueFetchFuture<'a> {
    Box::pin(async {
        Ok(TypedIssueSnapshot {
            body: actionable_issue_body(),
            labels: vec!["review".to_string(), "P1".to_string()],
        })
    })
}

fn actionable_no_label_issue_fetcher<'a>(
    _repo_slug: &'a str,
    _issue: u64,
    _github_token: Option<&'a str>,
) -> IssueFetchFuture<'a> {
    Box::pin(async {
        Ok(TypedIssueSnapshot {
            body: actionable_issue_body(),
            labels: vec![],
        })
    })
}

fn abstract_review_issue_fetcher<'a>(
    _repo_slug: &'a str,
    _issue: u64,
    _github_token: Option<&'a str>,
) -> IssueFetchFuture<'a> {
    Box::pin(async {
        Ok(TypedIssueSnapshot {
            body: abstract_issue_body(),
            labels: vec!["review".to_string()],
        })
    })
}

fn failing_issue_fetcher<'a>(
    _repo_slug: &'a str,
    _issue: u64,
    _github_token: Option<&'a str>,
) -> IssueFetchFuture<'a> {
    Box::pin(async { Err(anyhow::anyhow!("simulated GitHub failure")) })
}

async fn setup_issue_task_harness() -> anyhow::Result<(
    tempfile::TempDir,
    Arc<TaskStore>,
    EventStore,
    TaskId,
    CreateTaskRequest,
    RwLock<harness_skills::store::SkillStore>,
)> {
    let dir = tempfile::tempdir()?;
    let database_url = crate::test_helpers::test_database_url()?;
    let store =
        TaskStore::open_with_database_url(&dir.path().join("tasks.db"), Some(&database_url))
            .await?;
    let events = EventStore::new(dir.path()).await?;
    let task_id = TaskId::new();
    let mut task = crate::task_runner::TaskState::new(task_id.clone());
    task.task_kind = crate::task_runner::TaskKind::Issue;
    store.insert(&task).await;
    let req = CreateTaskRequest {
        issue: Some(988),
        turn_timeout_secs: 30,
        ..CreateTaskRequest::default()
    };
    let skills = RwLock::new(harness_skills::store::SkillStore::new());
    Ok((dir, store, events, task_id, req, skills))
}

#[tokio::test]
async fn actionable_review_issue_skip_is_promoted_to_plan() -> anyhow::Result<()> {
    let (dir, store, events, task_id, req, skills) = setup_issue_task_harness().await?;
    let agent = TriageStaticStreamAgent::new(
        "Looks abstract.\nCOMPLEXITY=low\nTRIAGE_REASON=agent_skip_review_label\nTRIAGE=SKIP",
    );

    let outcome = run_triage_plan_pipeline_with_issue_fetcher(
        &agent,
        &store,
        &task_id,
        988,
        "owner/repo",
        None,
        None,
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
        actionable_review_issue_fetcher,
    )
    .await?;

    assert!(matches!(
        outcome,
        TriagePlanPipelineOutcome::Continue {
            plan_output: Some(_),
            ..
        }
    ));

    let events = events
        .query(&EventFilters {
            hook: Some("triage_decision".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].detail.as_deref(), Some("result=promote"));
    assert_eq!(
        events[0].reason.as_deref(),
        Some(
            "actionable_issue_markers:file_line,recommended_section,acceptance_criteria,code_or_steps"
        )
    );
    Ok(())
}

#[tokio::test]
async fn actionable_issue_without_labels_skip_is_promoted_to_plan() -> anyhow::Result<()> {
    let (dir, store, events, task_id, req, skills) = setup_issue_task_harness().await?;
    let agent = TriageStaticStreamAgent::new(
        "Skip.\nCOMPLEXITY=medium\nTRIAGE_REASON=agent_skip\nTRIAGE=SKIP",
    );

    let outcome = run_triage_plan_pipeline_with_issue_fetcher(
        &agent,
        &store,
        &task_id,
        988,
        "owner/repo",
        None,
        None,
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
        actionable_no_label_issue_fetcher,
    )
    .await?;

    assert!(matches!(
        outcome,
        TriagePlanPipelineOutcome::Continue { .. }
    ));
    let events = events
        .query(&EventFilters {
            hook: Some("triage_decision".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events[0].detail.as_deref(), Some("result=promote"));
    assert!(
        events[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("actionable_issue_markers")),
        "missing actionable promotion reason: {:?}",
        events[0].reason
    );
    Ok(())
}

#[tokio::test]
async fn abstract_issue_skip_stays_skipped() -> anyhow::Result<()> {
    let (dir, store, events, task_id, req, skills) = setup_issue_task_harness().await?;
    let agent = TriageStaticStreamAgent::new("Not actionable.\nCOMPLEXITY=low\nTRIAGE=SKIP");

    let outcome = run_triage_plan_pipeline_with_issue_fetcher(
        &agent,
        &store,
        &task_id,
        988,
        "owner/repo",
        None,
        None,
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
        abstract_review_issue_fetcher,
    )
    .await?;

    assert!(matches!(outcome, TriagePlanPipelineOutcome::Skipped));
    let events = events
        .query(&EventFilters {
            hook: Some("triage_decision".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events[0].detail.as_deref(), Some("result=skip"));
    assert_eq!(
        events[0].reason.as_deref(),
        Some("non_actionable_issue_markers:none")
    );
    Ok(())
}

#[tokio::test]
async fn skip_on_review_label_true_keeps_legacy_skip_reason_for_non_actionable_issue(
) -> anyhow::Result<()> {
    let (dir, store, events, task_id, req, skills) = setup_issue_task_harness().await?;
    let agent = TriageStaticStreamAgent::new("Skip.\nCOMPLEXITY=low\nTRIAGE=SKIP");
    let triage_config = ProjectTriageConfig {
        skip_on_review_label: Some(true),
    };

    let outcome = run_triage_plan_pipeline_with_issue_fetcher(
        &agent,
        &store,
        &task_id,
        988,
        "owner/repo",
        None,
        Some(&triage_config),
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
        abstract_review_issue_fetcher,
    )
    .await?;

    assert!(matches!(outcome, TriagePlanPipelineOutcome::Skipped));
    let events = events
        .query(&EventFilters {
            hook: Some("triage_decision".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        events[0].reason.as_deref(),
        Some("review_label_skip_allowed_non_actionable")
    );
    Ok(())
}

#[tokio::test]
async fn github_fetch_failure_falls_back_to_agent_skip() -> anyhow::Result<()> {
    let (dir, store, events, task_id, req, skills) = setup_issue_task_harness().await?;
    let agent = TriageStaticStreamAgent::new(
        "Skip.\nCOMPLEXITY=low\nTRIAGE_REASON=agent_skip\nTRIAGE=SKIP",
    );

    let outcome = run_triage_plan_pipeline_with_issue_fetcher(
        &agent,
        &store,
        &task_id,
        988,
        "owner/repo",
        None,
        None,
        &HashMap::new(),
        dir.path(),
        &req,
        &skills,
        &events,
        failing_issue_fetcher,
    )
    .await?;

    assert!(matches!(outcome, TriagePlanPipelineOutcome::Skipped));
    let events = events
        .query(&EventFilters {
            hook: Some("triage_decision".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events[0].reason.as_deref(), Some("agent_skip"));
    Ok(())
}

#[test]
fn resolve_triage_decision_promotes_only_actionable_skip() {
    let actionable = TypedIssueSnapshot {
        body: actionable_issue_body(),
        labels: vec!["review".to_string()],
    };
    let resolved = resolve_triage_decision(
        prompts::TriageDecision::Skip,
        Some("agent_skip_review_label"),
        Some(&actionable),
        None,
    );
    assert_eq!(resolved.decision, prompts::TriageDecision::ProceedWithPlan);

    let abstract_issue = TypedIssueSnapshot {
        body: abstract_issue_body(),
        labels: vec!["review".to_string()],
    };
    let resolved = resolve_triage_decision(
        prompts::TriageDecision::Skip,
        Some("agent_skip_review_label"),
        Some(&abstract_issue),
        None,
    );
    assert_eq!(resolved.decision, prompts::TriageDecision::Skip);
    assert_eq!(resolved.reason, "agent_skip_review_label");
}
