//! Real execution path for a pinned agent-contract runtime job (Slice B).
//!
//! The dispatcher still holds contract commands behind the
//! `agent_contract_enforcement_unavailable` barrier until the assessment
//! slice ships, so no production dispatch reaches this path yet; the executor
//! wiring and the worker-tick integration tests keep it real and directly
//! callable instead of merely declared.
//!
//! Enforcement model: a conforming backend cannot switch its tool surface off
//! (codex-cli has no such flag), so the contract is enforced by construction
//! plus observation —
//! - the launch input is the pinned prompt alone: no prompt packet, no repo
//!   memory, no workflow document, no user config or rule files;
//! - the workspace is a fresh empty temp directory with no repository
//!   checkout (`workspace: ephemeral_empty`);
//! - the launch is deny-all-tools (`allowed_tools = []`), read-only sandbox,
//!   approvals never;
//! - the pinned output schema document is handed to the backend's structured
//!   output channel;
//! - the server records the whole event stream, and any tool, mutation,
//!   approval, or unknown event invalidates the attempt.

use crate::http::AppState;
use harness_core::agent::{AgentBackend, AgentEvent, AgentRequest, AGENT_OUTPUT_SCHEMA_PATH_ENV};
use harness_core::config::agents::{AgentPermissionMode, SandboxMode};
use harness_core::config::workflow::{
    agent_contract_output_schema_document, WorkflowAgentContract,
};
use harness_core::types::Item;
use harness_workflow::runtime::{ActivityErrorKind, ActivityResult, RuntimeJob};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

use super::agent_contract_enforcement::{
    ensure_backend_can_enforce_contract, turn_observation_artifact, PinnedJobAgentContract,
    TurnStreamObservations,
};
use super::data_helpers::activity_name;
use super::runtime_profile::{agent_name_for_runtime_kind, runtime_profile_for_job};

/// Everything the server observed and received from one contract attempt.
#[derive(Debug)]
pub(super) struct ContractAttempt {
    /// Final structured reply text from the backend.
    pub(super) output: String,
    /// Completed transcript items, recorded from the stream.
    pub(super) items: Vec<Item>,
    /// Server-observed stream facts for the whole attempt.
    pub(super) observations: TurnStreamObservations,
}

/// Structured verdict parsed from a contract attempt's reply.
pub(super) struct ContractVerdict {
    pub(super) outcome: String,
    pub(super) raw: Value,
}

/// Executes a contract-carrying runtime job end to end: capability preflight,
/// pinned launch, observation, and verdict validation. Preflight failures
/// (unregistered backend, missing capability claims, missing schema document)
/// return an explicit fatal `ActivityResult` so a contract job can never fall
/// through to the ordinary tool surface; launch errors bubble so the worker's
/// transient/fatal classification applies.
pub(super) async fn execute_contract_job(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    pinned: PinnedJobAgentContract,
) -> anyhow::Result<ActivityResult> {
    let activity = activity_name(job);
    let agent_name = agent_name_for_runtime_kind(job.runtime_kind)?;
    let Some(backend) = state.core.server.agent_registry.get(agent_name) else {
        anyhow::bail!("runtime agent `{agent_name}` is not registered");
    };
    if let Err(error) = ensure_backend_can_enforce_contract(backend.as_ref()) {
        return Ok(contract_preflight_failure(
            job,
            &activity,
            &error.to_string(),
        ));
    }
    if agent_contract_output_schema_document(&pinned.contract.output_schema).is_none() {
        return Ok(contract_preflight_failure(
            job,
            &activity,
            &format!(
                "output schema `{}` has no canonical schema document to enforce",
                pinned.contract.output_schema
            ),
        ));
    }
    let profile = runtime_profile_for_job(job)?;
    let attempt = execute_agent_contract_attempt(
        backend,
        &pinned,
        profile.model.clone(),
        profile.reasoning_effort.clone(),
    )
    .await?;
    Ok(contract_attempt_activity_result(
        &activity, &pinned, &attempt,
    ))
}

fn contract_preflight_failure(job: &RuntimeJob, activity: &str, reason: &str) -> ActivityResult {
    ActivityResult::failed(
        activity,
        format!(
            "Runtime job {} carries a pinned agent_contract that cannot be enforced.",
            job.id
        ),
        reason,
    )
    .with_error_kind(ActivityErrorKind::Fatal)
}

/// Launches one contract attempt against `backend` and records the whole
/// stream. This is the enforcement primitive itself; it performs the
/// capability preflight again so a direct caller can never skip it.
pub(super) async fn execute_agent_contract_attempt(
    backend: Arc<dyn AgentBackend>,
    pinned: &PinnedJobAgentContract,
    model: Option<String>,
    reasoning_effort: Option<String>,
) -> anyhow::Result<ContractAttempt> {
    ensure_backend_can_enforce_contract(backend.as_ref())?;
    let schema_document = agent_contract_output_schema_document(&pinned.contract.output_schema)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "output schema `{}` has no canonical schema document to enforce",
                pinned.contract.output_schema
            )
        })?;

    // The workspace stays literally empty: the schema file lives in its own
    // temp directory so nothing but the pinned prompt reaches the attempt.
    let workspace = tempfile::Builder::new()
        .prefix("harness-agent-contract-workspace-")
        .tempdir()?;
    let schema_dir = tempfile::Builder::new()
        .prefix("harness-agent-contract-schema-")
        .tempdir()?;
    let schema_path = schema_dir.path().join("output-schema.json");
    std::fs::write(&schema_path, schema_document)?;

    let request = AgentRequest {
        prompt: pinned.prompt.clone(),
        project_root: workspace.path().to_path_buf(),
        permission_mode: AgentPermissionMode::Scoped,
        allowed_tools: Some(Vec::new()),
        sandbox_mode: Some(SandboxMode::ReadOnly),
        approval_policy: Some("never".to_string()),
        model,
        reasoning_effort,
        env_vars: std::iter::once((
            AGENT_OUTPUT_SCHEMA_PATH_ENV.to_string(),
            schema_path.display().to_string(),
        ))
        .collect(),
        ..AgentRequest::default()
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let stream_backend = Arc::clone(&backend);
    let stream = tokio::spawn(async move { stream_backend.execute_stream(request, tx).await });

    let mut observations = TurnStreamObservations::default();
    let mut items = Vec::new();
    let mut output = String::new();
    while let Some(event) = rx.recv().await {
        observations.record_stream_item(&event);
        match event {
            AgentEvent::ItemCompleted { item } => items.push(item),
            AgentEvent::TurnCompleted { output: reply } => output = reply,
            _ => {}
        }
    }
    stream
        .await
        .map_err(|error| anyhow::anyhow!("contract attempt stream task panicked: {error}"))?
        .map_err(|error| anyhow::anyhow!("contract attempt launch failed: {error}"))?;

    Ok(ContractAttempt {
        output,
        items,
        observations,
    })
}

/// Converts an observed attempt into the job's `ActivityResult`: violations
/// invalidate the attempt, an unparsable or off-vocabulary verdict fails it,
/// and the server-authored observation artifact is attached either way.
/// Correction retries within `max_corrections` arrive with the assessment
/// slice.
pub(super) fn contract_attempt_activity_result(
    activity: &str,
    pinned: &PinnedJobAgentContract,
    attempt: &ContractAttempt,
) -> ActivityResult {
    let observation_artifact = turn_observation_artifact(1, &attempt.items, &attempt.observations);
    let violations = contract_violations(attempt);
    if !violations.is_empty() {
        return ActivityResult::failed(
            activity,
            "Agent contract attempt is invalid: the agent used a surface the pinned contract forbids.",
            format!("contract violations: {}", violations.join("; ")),
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(observation_artifact);
    }
    match parse_contract_verdict(&attempt.output, &pinned.contract) {
        Ok(verdict) => ActivityResult::succeeded(
            activity,
            format!("Agent contract verdict: {}.", verdict.outcome),
        )
        .with_artifact(observation_artifact)
        .with_artifact(harness_workflow::runtime::ActivityArtifact::new(
            "agent_contract_verdict",
            json!({
                "output_schema": pinned.contract.output_schema,
                "definition_hash": pinned.definition_hash,
                "outcome": verdict.outcome,
                "verdict": verdict.raw,
            }),
        )),
        Err(reason) => ActivityResult::failed(
            activity,
            "Agent contract attempt did not return a valid structured verdict.",
            reason,
        )
        .with_error_kind(ActivityErrorKind::Fatal)
        .with_artifact(observation_artifact),
    }
}

/// Everything observed during the attempt that the pinned contract forbids.
/// Fail closed: unknown event kinds count as violations because an
/// unrecognized surface cannot be proven benign.
pub(super) fn contract_violations(attempt: &ContractAttempt) -> Vec<String> {
    let mut violations = BTreeSet::new();
    for item in &attempt.items {
        match item {
            Item::ShellCommand { command, .. } => {
                violations.insert(format!("shell_command `{command}`"));
            }
            Item::ToolCall { name, .. } => {
                violations.insert(format!("tool_call `{name}`"));
            }
            Item::FileEdit { path, .. } => {
                violations.insert(format!("file_edit `{}`", path.display()));
            }
            Item::FileRead { path, .. } => {
                violations.insert(format!("file_read `{}`", path.display()));
            }
            Item::ApprovalRequest { action, .. } => {
                violations.insert(format!("approval_request `{action}`"));
            }
            Item::UserMessage { .. } | Item::AgentReasoning { .. } | Item::Error { .. } => {}
        }
    }
    // Stream-level kinds cover items that started but never completed.
    for kind in &attempt.observations.started_item_kinds {
        if !matches!(kind.as_str(), "user_message" | "agent_reasoning" | "error") {
            violations.insert(format!("started item of kind `{kind}`"));
        }
    }
    for kind in &attempt.observations.unknown_item_kinds {
        violations.insert(format!("unknown event kind `{kind}`"));
    }
    if attempt.observations.approval_requests > 0 {
        violations.insert(format!(
            "{} approval request(s)",
            attempt.observations.approval_requests
        ));
    }
    violations.into_iter().collect()
}

/// Validates the attempt's reply against the pinned contract: it must be a
/// JSON object naming the pinned output schema, with an `outcome` from the
/// pinned vocabulary and a non-empty `rationale`.
pub(super) fn parse_contract_verdict(
    output: &str,
    contract: &WorkflowAgentContract,
) -> Result<ContractVerdict, String> {
    let raw: Value = serde_json::from_str(output.trim())
        .map_err(|error| format!("reply is not valid JSON: {error}"))?;
    let schema = raw
        .get("schema")
        .and_then(Value::as_str)
        .ok_or_else(|| "reply is missing its `schema` field".to_string())?;
    if schema != contract.output_schema {
        return Err(format!(
            "reply names schema `{schema}` instead of the pinned `{}`",
            contract.output_schema
        ));
    }
    let outcome = raw
        .get("outcome")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|outcome| !outcome.is_empty())
        .ok_or_else(|| "reply is missing a non-empty `outcome`".to_string())?;
    if !contract
        .allowed_outcomes
        .iter()
        .any(|allowed| allowed == outcome)
    {
        return Err(format!(
            "outcome `{outcome}` is not in the pinned allowed_outcomes [{}]",
            contract.allowed_outcomes.join(", ")
        ));
    }
    if raw
        .get("rationale")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|rationale| !rationale.is_empty())
        .is_none()
    {
        return Err("reply is missing a non-empty `rationale`".to_string());
    }
    Ok(ContractVerdict {
        outcome: outcome.to_string(),
        raw,
    })
}

#[cfg(test)]
#[path = "agent_contract_attempt_tests.rs"]
mod tests;
