use super::*;
use harness_workflow::runtime::{
    RuntimeKind, RuntimeProfile, WorkflowCommandType, WorkflowRuntimeStore,
    PROMPT_TASK_IMPLEMENT_ACTIVITY,
};
use serde_json::{json, Value};
use std::collections::VecDeque;

struct SequencedPromptAgent {
    results: Mutex<VecDeque<Value>>,
    prompts: Mutex<Vec<String>>,
}

impl SequencedPromptAgent {
    fn new(results: impl IntoIterator<Item = Value>) -> Arc<Self> {
        Arc::new(Self {
            results: Mutex::new(results.into_iter().collect()),
            prompts: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl CodeAgent for SequencedPromptAgent {
    fn name(&self) -> &str {
        "sequenced-prompt-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.prompts.lock().await.push(req.prompt);
        Ok(empty_agent_response())
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.prompts.lock().await.push(req.prompt);
        let result = self.results.lock().await.pop_front().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "sequenced prompt agent has no result for this turn".to_string(),
            )
        })?;
        let content = format!(
            "runtime result\n\n```harness-activity-result\n{}\n```",
            result
        );
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning { content },
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

fn prompt_result(summary: &str, state: &str, subject: &str, revision: u64) -> Value {
    json!({
        "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "status": "succeeded",
        "summary": summary,
        "artifacts": [{
            "artifact_type": "tracker_snapshot",
            "artifact": {
                "subject": subject,
                "revision": revision
            }
        }],
        "signals": [{
            "signal_type": "external_state",
            "signal": {
                "state": state,
                "subject": subject
            }
        }],
        "validation": [{
            "command": "check external tracker",
            "status": "passed"
        }]
    })
}

async fn continuation_test_state(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    agent: Arc<SequencedPromptAgent>,
) -> anyhow::Result<Arc<AppState>> {
    std::fs::create_dir_all(project_root)?;
    init_fake_git_repo(project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\nworkspace:\n  strategy: source\n---\n",
    )?;
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent);
    make_test_state_with_workflow_runtime_and_registry(dir, project_root, registry).await
}

async fn submit_continuation_prompt(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    external_id: &str,
    no_progress_limit: u32,
) -> anyhow::Result<String> {
    let response = runtime_submission_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/submissions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "project": project_root.display().to_string(),
                        "prompt": format!("Watch {external_id} until its external state settles."),
                        "source": "runtime-e2e-test",
                        "external_id": external_id,
                        "continuation": {
                            "max_attempts": 5,
                            "attempt_delay_secs": 0,
                            "active_states": ["In Progress"],
                            "no_progress_limit": no_progress_limit
                        }
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let response = response_json(response).await?;
    assert_eq!(response["status"], "implementing");
    response["workflow_id"]
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("prompt submission response is missing workflow_id"))
}

async fn dispatch_and_run_prompt_attempt(
    state: &Arc<AppState>,
    workflow_id: &str,
) -> anyhow::Result<()> {
    let dispatch = super::background::run_runtime_command_dispatch_tick(
        state,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
        10,
    )
    .await?;
    assert_eq!(dispatch.enqueued, 1);
    assert_eq!(dispatch.skipped, 0);

    let worker = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        state,
        "prompt-continuation-e2e-worker",
        chrono::Duration::minutes(5),
    )
    .await?;
    if worker.succeeded != 1 {
        let store = state
            .core
            .workflow_runtime_store
            .as_ref()
            .expect("workflow runtime store should be configured");
        let mut failures = Vec::new();
        for command in store.commands_for(workflow_id).await? {
            for job in store.runtime_jobs_for_command(&command.id).await? {
                if job.status == harness_workflow::runtime::RuntimeJobStatus::Failed {
                    failures.push(job.output);
                }
            }
        }
        anyhow::bail!("worker tick {worker:?}; failed outputs: {failures:?}");
    }
    assert_eq!(worker.failed, 0);
    Ok(())
}

async fn prompt_attempt_count(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<usize> {
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .filter(|command| {
            command.command.command_type == WorkflowCommandType::EnqueueActivity
                && command.command.activity_name() == Some(PROMPT_TASK_IMPLEMENT_ACTIVITY)
        })
        .count())
}

#[tokio::test]
async fn prompt_continuation_runtime_reaches_second_agent_turn_with_attempt_context(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-settled");
    let agent = SequencedPromptAgent::new([
        prompt_result(
            "TEAM-123 remains active after the first check.",
            "In Progress",
            "TEAM-123",
            1,
        ),
        prompt_result("TEAM-123 is settled.", "Done", "TEAM-123", 2),
    ]);
    let state = continuation_test_state(dir.path(), &project_root, agent.clone()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow_id = submit_continuation_prompt(&state, &project_root, "TEAM-123", 3).await?;

    dispatch_and_run_prompt_attempt(&state, &workflow_id).await?;
    let after_first = store
        .get_instance(&workflow_id)
        .await?
        .expect("prompt workflow should remain persisted");
    assert_eq!(after_first.state, "implementing");
    assert_eq!(after_first.data["continuation"]["attempt"], 2);
    assert_eq!(
        after_first.data["continuation"]["last_external_state"],
        "In Progress"
    );

    dispatch_and_run_prompt_attempt(&state, &workflow_id).await?;
    let completed = store
        .get_instance(&workflow_id)
        .await?
        .expect("prompt workflow should remain persisted");
    assert_eq!(completed.state, "done");
    assert_eq!(prompt_attempt_count(store, &workflow_id).await?, 2);
    let prompts = agent.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    assert!(!prompts[0].contains("Continuation context:"));
    assert!(prompts[1].contains("Continuation context:"));
    assert!(prompts[1].contains("Attempt: 2"));
    assert!(prompts[1].contains("Previous external state: In Progress"));
    assert!(prompts[1]
        .contains("Previous attempt summary: TEAM-123 remains active after the first check."));
    Ok(())
}

#[tokio::test]
async fn prompt_continuation_runtime_blocks_repeated_same_state_and_meaningful_evidence(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-no-progress");
    let agent = SequencedPromptAgent::new([
        prompt_result(
            "TEAM-STALLED remains active after the first check.",
            "In Progress",
            "TEAM-STALLED",
            7,
        ),
        prompt_result(
            "A different summary must not count as progress.",
            "In Progress",
            "TEAM-STALLED",
            7,
        ),
    ]);
    let state = continuation_test_state(dir.path(), &project_root, agent.clone()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow_id = submit_continuation_prompt(&state, &project_root, "TEAM-STALLED", 1).await?;

    dispatch_and_run_prompt_attempt(&state, &workflow_id).await?;
    dispatch_and_run_prompt_attempt(&state, &workflow_id).await?;

    let blocked = store
        .get_instance(&workflow_id)
        .await?
        .expect("prompt workflow should remain persisted");
    assert_eq!(blocked.state, "blocked");
    assert_eq!(blocked.data["continuation"]["same_state_count"], 1);
    assert_eq!(prompt_attempt_count(store, &workflow_id).await?, 2);
    let prompts = agent.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("Attempt: 2"));
    Ok(())
}
