//! Runtime-side enforcement surface for pinned agent contracts.
//!
//! The dispatcher authorizes only an exact selected profile whose concrete
//! backend claims the required capabilities. This module supplies the
//! executor-level defense in depth: it extracts the pinned contract from a runtime job
//! fail-closed, verifies that the backend instance about to be launched
//! claims every enforcement capability, and records the per-attempt stream
//! observations (tool and mutation activity, unknown event kinds, approval
//! requests, model identity) that invalidate a violating attempt. The actual
//! launch lives in [`super::agent_contract_attempt`].

use harness_core::agent::{AgentBackend, AgentEvent, ModelIdentitySource};
use harness_core::config::workflow::{
    agent_contract_output_schema_document, WorkflowAgentContract,
};
use harness_core::types::Item;
use harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_TURN_OBSERVATIONS;
use harness_workflow::runtime::{
    ActivityArtifact, RuntimeJob, RuntimeProfile, WorkflowCommandRecord, WorkflowRuntimeStore,
    RUNTIME_PROFILE_SNAPSHOT_HASH_KEY,
};
use serde_json::{json, Value};

/// Pinned agent contract carried by a runtime job's command payload.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PinnedJobAgentContract {
    pub(super) contract: WorkflowAgentContract,
    pub(super) prompt: String,
    pub(super) input: Value,
    pub(super) definition_hash: String,
}

/// Extracts the pinned agent contract from `job.input.command`.
///
/// An absent `agent_contract` key means an ordinary activity. A present key —
/// including `null` — must parse into a complete, valid pinned contract with
/// its prompt and definition hash; anything else is an error so a malformed
/// payload can never select the ordinary (full-tool) execution path.
pub(super) fn pinned_agent_contract_for_job(
    job: &RuntimeJob,
) -> anyhow::Result<Option<PinnedJobAgentContract>> {
    let command = job.input.get("command");
    let activity = job
        .input
        .get("activity")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    extract_pinned_agent_contract(command, activity, &format!("runtime job {}", job.id))
}

pub(crate) async fn validate_pinned_agent_contract_command(
    store: &WorkflowRuntimeStore,
    command: &WorkflowCommandRecord,
) -> anyhow::Result<bool> {
    let command_has_contract = command.command.command.get("agent_contract").is_some();
    let instance = store
        .get_instance(&command.workflow_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "workflow command references missing instance '{}'",
                command.workflow_id
            )
        })?;
    let persisted = store
        .get_definition(&instance.definition_id, instance.definition_version)
        .await?;
    if !command_has_contract
        && instance.data.get("definition_hash").is_none()
        && !persisted
            .as_ref()
            .is_some_and(harness_workflow::runtime::is_persisted_declarative_definition)
    {
        return Ok(false);
    }
    let persisted = persisted.ok_or_else(|| {
        anyhow::anyhow!(
            "workflow instance '{}' references missing definition '{}@{}'",
            instance.id,
            instance.definition_id,
            instance.definition_version
        )
    })?;
    let definition =
        harness_workflow::runtime::hydrate_persisted_declarative_definition(&persisted)?;
    let has_contract = harness_workflow::runtime::validate_declarative_agent_contract_command(
        &definition,
        &instance,
        &command.command,
    )?;
    if has_contract {
        extract_pinned_agent_contract(
            Some(&command.command.command),
            command.command.runtime_activity_key(),
            "workflow command",
        )?;
    }
    Ok(has_contract)
}

pub(crate) async fn validate_pinned_agent_contract_job(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
) -> anyhow::Result<bool> {
    let command = store
        .get_command(&job.command_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job references a missing workflow command"))?;
    let command_has_contract = validate_pinned_agent_contract_command(store, &command).await?;
    let job_has_contract = job.input.pointer("/command/agent_contract").is_some();
    if !command_has_contract {
        if job_has_contract {
            anyhow::bail!("ordinary workflow command cannot authorize an agent contract job");
        }
        return Ok(false);
    }
    if pinned_agent_contract_for_job(job)?.is_none() {
        anyhow::bail!("runtime job lost the agent contract pinned by its workflow command");
    }
    if job.input.get("workflow_id").and_then(Value::as_str) != Some(&command.workflow_id)
        || job.input.get("command_id").and_then(Value::as_str) != Some(&command.id)
        || job.input.get("command_type")
            != Some(&serde_json::to_value(command.command.command_type)?)
        || job.input.get("dedupe_key").and_then(Value::as_str) != Some(&command.command.dedupe_key)
        || job.input.get("activity").and_then(Value::as_str)
            != Some(command.command.runtime_activity_key())
        || job.input.get("command") != Some(&command.command.command)
    {
        anyhow::bail!("runtime job agent contract snapshot does not match its workflow command");
    }
    validate_agent_contract_runtime_profile(job)?;
    Ok(true)
}

fn validate_agent_contract_runtime_profile(job: &RuntimeJob) -> anyhow::Result<()> {
    let profile_value = job
        .input
        .get("runtime_profile")
        .ok_or_else(|| anyhow::anyhow!("agent contract runtime job lost its profile snapshot"))?;
    let profile_hash = job
        .input
        .get(RUNTIME_PROFILE_SNAPSHOT_HASH_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("agent contract runtime job lost its profile hash"))?;
    if harness_workflow::runtime::stable_remote_fact_hash(profile_value) != profile_hash {
        anyhow::bail!("agent contract runtime profile snapshot hash does not match");
    }
    let profile: RuntimeProfile = serde_json::from_value(profile_value.clone())?;
    if profile.name != job.runtime_profile || profile.kind != job.runtime_kind {
        anyhow::bail!("agent contract runtime profile snapshot does not match the runtime job");
    }
    Ok(())
}

fn extract_pinned_agent_contract(
    command: Option<&Value>,
    activity: &str,
    context: &str,
) -> anyhow::Result<Option<PinnedJobAgentContract>> {
    let Some(contract_value) = command.and_then(|command| command.get("agent_contract")) else {
        return Ok(None);
    };
    let contract: WorkflowAgentContract =
        serde_json::from_value(contract_value.clone()).map_err(|error| {
            anyhow::anyhow!("{context} carries an unparseable agent_contract payload: {error}")
        })?;
    contract.validate(activity)?;
    if agent_contract_output_schema_document(&contract.output_schema).is_none() {
        anyhow::bail!(
            "output schema '{}' has no canonical enforcement document",
            contract.output_schema
        );
    }
    let prompt = command
        .and_then(|command| command.get("prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("{context} carries an agent_contract without its pinned prompt")
        })?;
    let definition_hash = command
        .and_then(|command| command.get("definition_hash"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{context} carries an agent_contract without its pinned definition hash"
            )
        })?;
    let input = command
        .and_then(|command| command.get("agent_contract_input"))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!("{context} carries an agent_contract without its pinned input envelope")
        })?;
    let pinned = PinnedJobAgentContract {
        contract,
        prompt: prompt.to_string(),
        input,
        definition_hash: definition_hash.to_string(),
    };
    super::agent_contract_prompt::validate_contract_input(&pinned)?;
    Ok(Some(pinned))
}

/// Whether the backend instance the registry returned — the same object the
/// attempt would launch — claims every capability the contract's no-tool,
/// no-mutation, ephemeral-empty-workspace constraints require. Fail closed:
/// an unclaimed capability rejects the attempt before anything is spawned.
pub(crate) fn ensure_backend_can_enforce_contract(
    backend: &dyn AgentBackend,
) -> anyhow::Result<()> {
    let missing = backend
        .agent_contract_capabilities()
        .missing_for_enforcement();
    if missing.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "agent backend `{}` cannot enforce an agent_contract; it does not claim: {}",
        backend.id(),
        missing.join(", ")
    )
}

/// Server-observed stream facts for one runtime attempt, recorded directly
/// from the agent event stream (not from agent claims).
#[derive(Debug, Default)]
pub(crate) struct TurnStreamObservations {
    pub(crate) reported_models: Vec<(String, ModelIdentitySource)>,
    pub(crate) started_item_kinds: Vec<String>,
    pub(crate) unknown_item_kinds: Vec<String>,
    pub(crate) approval_requests: u32,
    pub(crate) tool_output_deltas: u32,
}

/// Item kinds the runtime recognizes as carrying no tool or mutation surface.
const OBSERVED_BENIGN_ITEM_KINDS: [&str; 2] = ["message", "reasoning"];

impl TurnStreamObservations {
    pub(crate) fn record_stream_item(&mut self, item: &AgentEvent) {
        match item {
            AgentEvent::ModelReported { model, source } => {
                self.reported_models.push((model.clone(), *source));
            }
            AgentEvent::ItemStartedKind { item_type } => {
                if !OBSERVED_BENIGN_ITEM_KINDS.contains(&item_type.as_str()) {
                    self.unknown_item_kinds.push(item_type.clone());
                }
            }
            AgentEvent::ItemStarted { item } => {
                self.started_item_kinds.push(item_kind_label(item));
            }
            AgentEvent::ToolCall { name, .. } => {
                self.started_item_kinds.push(format!("tool_call:{name}"));
            }
            AgentEvent::ApprovalRequest { .. } => {
                self.approval_requests = self.approval_requests.saturating_add(1);
            }
            AgentEvent::ToolOutputDelta { .. } => {
                self.tool_output_deltas = self.tool_output_deltas.saturating_add(1);
            }
            _ => {}
        }
    }
}

fn item_kind_label(item: &Item) -> String {
    match item {
        Item::UserMessage { .. } => "user_message",
        Item::AgentReasoning { .. } => "agent_reasoning",
        Item::ShellCommand { .. } => "shell_command",
        Item::FileEdit { .. } => "file_edit",
        Item::FileRead { .. } => "file_read",
        Item::ToolCall { .. } => "tool_call",
        Item::ApprovalRequest { .. } => "approval_request",
        Item::Error { .. } => "error",
    }
    .to_string()
}

/// Server-authored artifact describing everything security-relevant the
/// runtime observed during one contract attempt. Attached only to
/// agent-contract activity results — ordinary activities are unchanged —
/// and kept per attempt so a later correction attempt can never erase an
/// earlier observation.
pub(super) fn turn_observation_artifact(
    attempt_number: u32,
    turn_items: &[Item],
    observations: &TurnStreamObservations,
) -> ActivityArtifact {
    let mut tool_surface_items: Vec<Value> = Vec::new();
    for item in turn_items {
        match item {
            Item::ShellCommand { command, .. } => {
                tool_surface_items.push(json!({"kind": "shell_command", "command": command}));
            }
            Item::ToolCall { name, .. } => {
                tool_surface_items.push(json!({"kind": "tool_call", "name": name}));
            }
            Item::FileEdit { path, .. } => {
                tool_surface_items.push(json!({"kind": "file_edit", "path": path}));
            }
            Item::FileRead { path, .. } => {
                tool_surface_items.push(json!({"kind": "file_read", "path": path}));
            }
            Item::ApprovalRequest { action, .. } => {
                tool_surface_items.push(json!({"kind": "approval_request", "action": action}));
            }
            Item::UserMessage { .. } | Item::AgentReasoning { .. } | Item::Error { .. } => {}
        }
    }
    ActivityArtifact::new(
        ARTIFACT_RUNTIME_TURN_OBSERVATIONS,
        json!({
            "attempt_number": attempt_number,
            "tool_surface_items": tool_surface_items,
            "started_item_kinds": observations.started_item_kinds,
            "unknown_item_kinds": observations.unknown_item_kinds,
            "approval_requests": observations.approval_requests,
            "tool_output_deltas": observations.tool_output_deltas,
            "reported_models": observations
                .reported_models
                .iter()
                .map(|(model, source)| json!({"model": model, "source": source}))
                .collect::<Vec<_>>(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_runtime_worker::agent_contract_attempt::{
        contract_violations, ContractAttempt,
    };
    use harness_workflow::runtime::RuntimeKind;

    fn contract_value() -> Value {
        json!({
            "input_schema": "harness.semantic_activity_input.v1",
            "output_schema": "harness.semantic_verdict.v1",
            "allowed_outcomes": ["small", "large"],
            "tools": "none",
            "mutation": "forbidden",
            "workspace": "ephemeral_empty",
            "fresh_context": true,
        })
    }

    fn contract_input_value() -> Value {
        let contract: WorkflowAgentContract =
            serde_json::from_value(contract_value()).expect("fixture contract");
        let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(
            &serde_json::to_value(contract).expect("serialized fixture contract"),
        );
        json!({
            "schema": "harness.semantic_activity_input.v1",
            "subject": {"kind": "issue", "identity": "owner/repo#126"},
            "facts": {"changed_files": ["src/lib.rs"]},
            "provenance": {"/changed_files": "server"},
            "contract_hash": contract_hash,
        })
    }

    fn contract_job(command: Value) -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "classify_scope", "command": command }),
        )
    }

    #[test]
    fn absent_contract_key_selects_the_ordinary_path() {
        let job = contract_job(json!({ "activity": "classify_scope" }));
        assert!(pinned_agent_contract_for_job(&job)
            .expect("absent contract is ordinary")
            .is_none());
    }

    #[test]
    fn null_or_malformed_contract_fails_instead_of_falling_through() {
        for contract in [json!(null), json!({"tools": "none"}), json!("contract")] {
            let job = contract_job(json!({
                "activity": "classify_scope",
                "agent_contract": contract,
                "prompt": "Classify only the supplied facts.",
                "definition_hash": "sha256:abc",
            }));
            assert!(
                pinned_agent_contract_for_job(&job).is_err(),
                "a present but unparseable contract must never select a path silently: {contract}"
            );
        }
    }

    #[test]
    fn missing_prompt_definition_hash_or_input_fails() {
        let without_prompt = contract_job(json!({
            "activity": "classify_scope",
            "agent_contract": contract_value(),
            "definition_hash": "sha256:abc",
            "agent_contract_input": contract_input_value(),
        }));
        assert!(pinned_agent_contract_for_job(&without_prompt).is_err());

        let without_hash = contract_job(json!({
            "activity": "classify_scope",
            "agent_contract": contract_value(),
            "prompt": "Classify only the supplied facts.",
            "agent_contract_input": contract_input_value(),
        }));
        assert!(pinned_agent_contract_for_job(&without_hash).is_err());

        let without_input = contract_job(json!({
            "activity": "classify_scope",
            "agent_contract": contract_value(),
            "prompt": "Classify only the supplied facts.",
            "definition_hash": "sha256:abc",
        }));
        assert!(pinned_agent_contract_for_job(&without_input).is_err());
    }

    #[test]
    fn complete_pinned_contract_extracts() {
        let job = contract_job(json!({
            "activity": "classify_scope",
            "agent_contract": contract_value(),
            "prompt": "Classify only the supplied facts.",
            "definition_hash": "sha256:abc",
            "agent_contract_input": contract_input_value(),
        }));
        let pinned = pinned_agent_contract_for_job(&job)
            .expect("valid contract extracts")
            .expect("contract present");
        assert_eq!(pinned.prompt, "Classify only the supplied facts.");
        assert_eq!(pinned.definition_hash, "sha256:abc");
        assert_eq!(pinned.contract.allowed_outcomes, vec!["small", "large"]);
    }

    #[test]
    fn mismatched_contract_hash_fails_during_job_extraction() {
        let mut input = contract_input_value();
        input["contract_hash"] = json!("sha256:wrong");
        let job = contract_job(json!({
            "activity": "classify_scope",
            "agent_contract": contract_value(),
            "prompt": "Classify only the supplied facts.",
            "definition_hash": "sha256:abc",
            "agent_contract_input": input,
        }));

        assert!(pinned_agent_contract_for_job(&job).is_err());
    }

    #[test]
    fn substituted_runtime_profile_fails_its_creation_snapshot_hash() {
        let profile = harness_workflow::runtime::RuntimeProfile::new(
            "codex-contract",
            RuntimeKind::CodexExec,
        );
        let mut job = RuntimeJob::pending(
            "command-profile",
            RuntimeKind::CodexExec,
            "codex-contract",
            json!({"runtime_profile": profile}),
        );
        job.input["runtime_profile"]["model"] = json!("substituted-model");

        let error = validate_agent_contract_runtime_profile(&job)
            .expect_err("a substituted profile must fail before execution");

        assert!(error.to_string().contains("snapshot hash does not match"));
    }

    #[test]
    fn backend_without_contract_capability_claims_is_rejected() {
        struct UnclaimedBackend;
        #[async_trait::async_trait]
        impl AgentBackend for UnclaimedBackend {
            fn name(&self) -> &str {
                "unclaimed"
            }
        }

        let error = ensure_backend_can_enforce_contract(&UnclaimedBackend)
            .expect_err("a backend claiming nothing must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("cannot enforce an agent_contract"),
            "{message}"
        );
        assert!(message.contains("prompt_only_launch"), "{message}");
        assert!(message.contains("pinned_output_schema"), "{message}");
        assert!(message.contains("attempt_observation_stream"), "{message}");
    }

    #[test]
    fn backend_claiming_every_capability_is_accepted() {
        struct ClaimingBackend;
        #[async_trait::async_trait]
        impl AgentBackend for ClaimingBackend {
            fn name(&self) -> &str {
                "claiming"
            }
            fn agent_contract_capabilities(
                &self,
            ) -> harness_core::agent::AgentContractCapabilities {
                harness_core::agent::AgentContractCapabilities {
                    prompt_only_launch: true,
                    pinned_output_schema: true,
                    attempt_observation_stream: true,
                }
            }
        }

        assert!(ensure_backend_can_enforce_contract(&ClaimingBackend).is_ok());
    }

    #[test]
    fn stream_observations_record_models_unknown_kinds_and_approvals() {
        let mut observations = TurnStreamObservations::default();
        observations.record_stream_item(&AgentEvent::ModelReported {
            model: "gpt-5.2-codex".to_string(),
            source: ModelIdentitySource::LaunchDerived,
        });
        observations.record_stream_item(&AgentEvent::ItemStartedKind {
            item_type: "message".to_string(),
        });
        observations.record_stream_item(&AgentEvent::ItemStartedKind {
            item_type: "novel_side_effect".to_string(),
        });
        observations.record_stream_item(&AgentEvent::ApprovalRequest {
            id: "approval-1".to_string(),
            command: "rm -rf".to_string(),
        });
        observations.record_stream_item(&AgentEvent::ItemStarted {
            item: Item::ShellCommand {
                command: "pwd".to_string(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            },
        });

        assert_eq!(
            observations.reported_models,
            vec![(
                "gpt-5.2-codex".to_string(),
                ModelIdentitySource::LaunchDerived
            )]
        );
        assert_eq!(observations.unknown_item_kinds, vec!["novel_side_effect"]);
        assert_eq!(observations.approval_requests, 1);
        assert_eq!(observations.started_item_kinds, vec!["shell_command"]);
    }

    #[test]
    fn turn_observation_artifact_reports_every_tool_surface() {
        let mut observations = TurnStreamObservations::default();
        observations.record_stream_item(&AgentEvent::ItemStartedKind {
            item_type: "novel_side_effect".to_string(),
        });
        let items = vec![
            Item::AgentReasoning {
                content: "thinking".to_string(),
            },
            Item::ShellCommand {
                command: "cargo test".to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
            Item::ToolCall {
                name: "fetch_url".to_string(),
                input: json!({}),
                output: None,
            },
        ];
        let artifact = turn_observation_artifact(2, &items, &observations);
        assert_eq!(artifact.artifact_type, ARTIFACT_RUNTIME_TURN_OBSERVATIONS);
        assert_eq!(artifact.artifact["attempt_number"], 2);
        let tool_surface = artifact.artifact["tool_surface_items"]
            .as_array()
            .expect("tool surface array");
        assert_eq!(tool_surface.len(), 2);
        assert_eq!(
            artifact.artifact["unknown_item_kinds"],
            json!(["novel_side_effect"])
        );
    }

    #[test]
    fn tool_output_deltas_are_contract_violations_and_artifacted() {
        let mut observations = TurnStreamObservations::default();
        observations.record_stream_item(&AgentEvent::ToolOutputDelta {
            item_id: "item-1".to_string(),
            text: "command output".to_string(),
        });
        let attempt = ContractAttempt {
            output: "{}".to_string(),
            items: Vec::new(),
            observations,
        };

        assert_eq!(
            contract_violations(&attempt),
            vec!["1 tool output delta(s)"]
        );
        let artifact = turn_observation_artifact(1, &attempt.items, &attempt.observations);
        assert_eq!(artifact.artifact["tool_output_deltas"], 1);
    }
}
