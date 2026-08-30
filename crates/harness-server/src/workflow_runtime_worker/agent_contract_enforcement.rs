//! Runtime-side enforcement surface for pinned agent contracts (Slice B).
//!
//! The dispatcher defers agent-contract commands behind the
//! `agent_contract_enforcement_unavailable` barrier until the assessment
//! slice ships. This module supplies the executor-level primitives behind
//! that barrier: it extracts the pinned contract from a runtime job
//! fail-closed, verifies that the backend instance about to be launched
//! claims every enforcement capability, and records the per-attempt stream
//! observations (tool and mutation activity, unknown event kinds, approval
//! requests, model identity) that invalidate a violating attempt. The actual
//! launch lives in [`super::agent_contract_attempt`].

use harness_core::agent::{AgentBackend, AgentEvent, ModelIdentitySource};
use harness_core::config::workflow::WorkflowAgentContract;
use harness_core::types::Item;
use harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_TURN_OBSERVATIONS;
use harness_workflow::runtime::{ActivityArtifact, RuntimeJob};
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
    let Some(contract_value) = command.and_then(|command| command.get("agent_contract")) else {
        return Ok(None);
    };
    let contract: WorkflowAgentContract =
        serde_json::from_value(contract_value.clone()).map_err(|error| {
            anyhow::anyhow!(
                "runtime job {} carries an unparseable agent_contract payload: {error}",
                job.id
            )
        })?;
    let activity = job
        .input
        .get("activity")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    contract.validate(activity)?;
    let prompt = command
        .and_then(|command| command.get("prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime job {} carries an agent_contract without its pinned prompt",
                job.id
            )
        })?;
    let definition_hash = command
        .and_then(|command| command.get("definition_hash"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime job {} carries an agent_contract without its pinned definition hash",
                job.id
            )
        })?;
    let input = command
        .and_then(|command| command.get("agent_contract_input"))
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime job {} carries an agent_contract without its pinned input envelope",
                job.id
            )
        })?;
    Ok(Some(PinnedJobAgentContract {
        contract,
        prompt: prompt.to_string(),
        input,
        definition_hash: definition_hash.to_string(),
    }))
}

/// Whether the backend instance the registry returned — the same object the
/// attempt would launch — claims every capability the contract's no-tool,
/// no-mutation, ephemeral-empty-workspace constraints require. Fail closed:
/// an unclaimed capability rejects the attempt before anything is spawned.
pub(super) fn ensure_backend_can_enforce_contract(
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
        json!({
            "schema": "harness.semantic_activity_input.v1",
            "subject": {"kind": "issue", "identity": "owner/repo#126"},
            "facts": {"changed_files": ["src/lib.rs"]},
            "provenance": {"/changed_files": "server"},
            "contract_hash": "sha256:contract",
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
}
