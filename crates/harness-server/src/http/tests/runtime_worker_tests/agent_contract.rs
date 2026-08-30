//! Worker-tick behavior for pinned agent contracts (Slice B): capability
//! preflight, the pinned deny-all launch in an empty ephemeral workspace, and
//! observation-based invalidation.

use super::*;
use harness_core::agent::AGENT_OUTPUT_SCHEMA_PATH_ENV;
use std::sync::Arc;

/// Contract-conforming scripted backend: claims every enforcement capability,
/// records the launch request and workspace state, and replays a script.
struct ContractStreamAgent {
    script: Mutex<Vec<harness_core::agent::AgentEvent>>,
    requests: Mutex<Vec<AgentRequest>>,
    workspace_entry_counts: Mutex<Vec<usize>>,
}

impl ContractStreamAgent {
    fn new(script: Vec<harness_core::agent::AgentEvent>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
            workspace_entry_counts: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl CodeAgent for ContractStreamAgent {
    fn name(&self) -> &str {
        "contract-stream-agent"
    }

    fn agent_contract_capabilities(&self) -> harness_core::agent::AgentContractCapabilities {
        harness_core::agent::AgentContractCapabilities {
            prompt_only_launch: true,
            pinned_output_schema: true,
            attempt_observation_stream: true,
        }
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        let entries = std::fs::read_dir(&req.project_root)
            .map(|entries| entries.count())
            .unwrap_or(usize::MAX);
        self.workspace_entry_counts.lock().await.push(entries);
        self.requests.lock().await.push(req);
        let script = std::mem::take(&mut *self.script.lock().await);
        for event in script {
            tx.send(event).await.map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(error.to_string())
            })?;
        }
        Ok(())
    }
}

fn contract_command_payload() -> serde_json::Value {
    let contract: harness_core::config::workflow::WorkflowAgentContract =
        serde_json::from_value(serde_json::json!({
            "input_schema": "harness.semantic_activity_input.v1",
            "output_schema": "harness.semantic_verdict.v1",
            "allowed_outcomes": ["small", "large"],
            "tools": "none",
            "mutation": "forbidden",
            "workspace": "ephemeral_empty",
            "fresh_context": true,
        }))
        .expect("contract fixture should deserialize");
    let contract = serde_json::to_value(contract).expect("contract fixture should serialize");
    let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(&contract);
    serde_json::json!({
        "activity": "classify_scope",
        "agent_contract": contract,
        "prompt": "Classify only the supplied facts.",
        "agent_contract_input": {
            "schema": "harness.semantic_activity_input.v1",
            "subject": {"kind": "issue", "identity": "owner/repo#126"},
            "facts": {"changed_files": ["src/lib.rs"]},
            "provenance": {"/changed_files": "server"},
            "contract_hash": contract_hash,
        },
        "definition_hash": "sha256:pinned",
    })
}

fn valid_verdict_reply() -> String {
    serde_json::json!({
        "schema": "harness.semantic_verdict.v1",
        "outcome": "small",
        "rationale": "Touches a single function.",
        "evidence_refs": [],
    })
    .to_string()
}

async fn enqueue_contract_job(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    project_root: &std::path::Path,
) -> anyhow::Result<harness_workflow::runtime::RuntimeJob> {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:126"),
    )
    .with_id("issue-126")
    .with_classified_data(
        serde_json::json!({
            "project_id": project_root,
            "repo": "owner/repo",
            "issue_number": 126,
        }),
        harness_workflow::runtime::DataProvenance::Server,
    );
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    let command_payload = contract_command_payload();
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::EnqueueActivity,
        "classify-126",
        command_payload.clone(),
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexExec,
    );
    runtime_profile.timeout_secs = Some(30);
    store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "codex-default",
            serde_json::json!({
                "workflow_id": workflow.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key,
                "command": command_payload,
                "activity": "classify_scope",
                "runtime_profile": runtime_profile,
            }),
        )
        .await
}

/// A contract job on a backend that claims no enforcement capabilities must
/// fail explicitly at preflight without spawning any agent process. The
/// dispatcher barrier that normally holds such commands must not be the only
/// line of defense.
#[tokio::test]
async fn contract_job_on_unclaiming_backend_fails_without_spawning() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    // RuntimeStreamAgent claims no contract capabilities (the default).
    let agent = RuntimeStreamAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
    let state =
        make_test_state_with_workflow_runtime_and_registry(dir.path(), &project_root, registry)
            .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let runtime_job = enqueue_contract_job(store, &project_root).await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.failed, 1, "contract job must fail explicitly");
    assert!(
        agent.prompts.lock().await.is_empty(),
        "no agent process may be spawned for an unenforceable contract job"
    );
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Failed
    );
    let output: harness_workflow::runtime::ActivityResult =
        serde_json::from_value(completed.output.clone().expect("failed job carries output"))?;
    let error = output.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("cannot enforce an agent_contract"),
        "{error}"
    );
    assert!(error.contains("prompt_only_launch"), "{error}");
    Ok(())
}

/// A contract job on a conforming backend executes the real attempt: pinned
/// pinned prompt and input only, deny-all launch, empty ephemeral workspace outside the
/// repository, pinned schema document handed to the backend, and a structured
/// verdict recorded on the succeeded result.
#[tokio::test]
async fn contract_job_executes_pinned_attempt_on_conforming_backend() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let agent = ContractStreamAgent::new(vec![harness_core::agent::AgentEvent::TurnCompleted {
        output: valid_verdict_reply(),
    }]);
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
    let state =
        make_test_state_with_workflow_runtime_and_registry(dir.path(), &project_root, registry)
            .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let runtime_job = enqueue_contract_job(store, &project_root).await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        tick.succeeded, 1,
        "clean contract attempt must succeed; persisted status={:?}, output={:?}",
        completed.status, completed.output
    );

    let requests = agent.requests.lock().await;
    assert_eq!(requests.len(), 1, "exactly one attempt launch");
    let request = &requests[0];
    let payload = contract_command_payload();
    let expected_prompt = format!(
        "Classify only the supplied facts.\n\nAgent contract input (JSON):\n{}",
        serde_json::to_string_pretty(&payload["agent_contract_input"])?
    );
    assert_eq!(request.prompt, expected_prompt);
    assert_eq!(request.timeout_secs, Some(30));
    assert_eq!(
        request.allowed_tools.as_deref(),
        Some(&[][..]),
        "the launch is deny-all-tools"
    );
    assert_eq!(
        request.sandbox_mode,
        Some(harness_core::config::agents::SandboxMode::ReadOnly)
    );
    assert_eq!(request.approval_policy.as_deref(), Some("never"));
    assert!(
        !request.project_root.starts_with(&project_root),
        "the contract workspace must not be a repository checkout"
    );
    assert_eq!(
        agent.workspace_entry_counts.lock().await[0],
        0,
        "the contract workspace must be empty at launch"
    );
    assert!(
        request.env_vars.contains_key(AGENT_OUTPUT_SCHEMA_PATH_ENV),
        "the pinned output schema is handed to the backend"
    );

    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Succeeded
    );
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .clone()
            .expect("succeeded job carries output"),
    )?;
    let verdict = output
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "agent_contract_verdict")
        .expect("verdict artifact attached");
    assert_eq!(verdict.artifact["outcome"], "small");
    assert!(
        output.artifacts.iter().any(|artifact| {
            artifact.artifact_type
                == harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_TURN_OBSERVATIONS
        }),
        "the server-observed attempt record is attached"
    );
    Ok(())
}

/// Any tool-surface activity observed during a contract attempt invalidates
/// it, even when the reply itself is a valid verdict.
#[tokio::test]
async fn contract_attempt_with_tool_activity_is_invalidated() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let agent = ContractStreamAgent::new(vec![
        harness_core::agent::AgentEvent::ItemCompleted {
            item: harness_core::types::Item::ShellCommand {
                command: "uname -a".to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
        },
        harness_core::agent::AgentEvent::TurnCompleted {
            output: valid_verdict_reply(),
        },
    ]);
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
    let state =
        make_test_state_with_workflow_runtime_and_registry(dir.path(), &project_root, registry)
            .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let runtime_job = enqueue_contract_job(store, &project_root).await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;
    assert_eq!(tick.failed, 1, "a violating attempt must be invalidated");

    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Failed
    );
    let output: harness_workflow::runtime::ActivityResult =
        serde_json::from_value(completed.output.clone().expect("failed job carries output"))?;
    let error = output.error.as_deref().unwrap_or_default();
    assert!(error.contains("contract violations"), "{error}");
    assert!(error.contains("shell_command `uname -a`"), "{error}");
    Ok(())
}
