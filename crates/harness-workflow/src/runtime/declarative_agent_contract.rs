//! Agent-contract resolution for declarative workflow definitions.
//!
//! An `agent_contract` on a Workflow activity declares the generic execution
//! contract for a bounded, no-tool semantic activity. This module validates
//! referenced contracts at compile time and pins the resolved contract into
//! the `EnqueueActivity` command payload so the runtime job snapshot always
//! carries the exact contract the instance was pinned to.

use super::declarative::DeclarativeWorkflowDefinition;
use super::model::{
    ActivityResult, RuntimeKind, WorkflowCommand, WorkflowCommandType, WorkflowInstance,
};
use harness_core::config::workflow::{
    validate_agent_contract_input, validate_agent_contract_output, DeclaredState,
    WorkflowActivityPolicy, WorkflowAgentContract, WorkflowDefinitionPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const AGENT_CONTRACT_ASSESSMENT_SCHEMA: &str = "harness.agent_contract_assessment.v1";
pub const AGENT_CONTRACT_ASSESSMENT_ARTIFACT: &str = "agent_contract_assessment";
pub const AGENT_CONTRACT_VERDICT_ARTIFACT: &str = "agent_contract_verdict";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentContractAssessment {
    schema: String,
    assessment_id: String,
    activity: String,
    definition_hash: String,
    contract_hash: String,
    input_hash: String,
    runtime_job_id: String,
    command_id: String,
    runtime_profile: String,
    runtime_kind: RuntimeKind,
    outcome: String,
    verdict: Value,
    budget: AgentContractAssessmentBudget,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentContractAssessmentBudget {
    max_primary_attempts: u32,
    max_corrections: u32,
    primary_attempts_used: u32,
    corrections_used: u32,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AgentContractAttemptFact {
    primary_attempt: u32,
    correction_attempt: u32,
}

/// One activity's complete pinned agent execution policy: the contract plus
/// the effective prompt. Both participate in the definition identity and the
/// persisted metadata, so execution never rereads the mutable `WORKFLOW.md`
/// for a pinned instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PinnedAgentContractActivity {
    pub prompt: String,
    pub contract: WorkflowAgentContract,
}

/// Validates the single server-authored assessment carried by a completed
/// contract activity and returns the exact outcome used for `on_signal`
/// routing. Every identity and hash is checked against the pinned completion
/// event so replay never consults mutable workflow configuration or model
/// output outside the persisted event.
pub(crate) fn validated_agent_contract_assessment_outcome(
    definition: &DeclarativeWorkflowDefinition,
    activity: &str,
    event: &super::model::WorkflowEvent,
    result: &ActivityResult,
) -> anyhow::Result<Option<String>> {
    let Some(pinned) = definition.agent_contract(activity) else {
        return Ok(None);
    };
    let command: WorkflowCommand =
        serde_json::from_value(
            event.event.get("command").cloned().ok_or_else(|| {
                anyhow::anyhow!("agent contract completion is missing its command")
            })?,
        )?;
    let command_id = event
        .event
        .get("command_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("agent contract completion is missing command_id"))?;
    let runtime_job_id = event
        .event
        .get("runtime_job_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("agent contract completion is missing runtime_job_id"))?;
    let runtime_job_profile = event
        .event
        .get("runtime_job_profile")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("agent contract completion is missing runtime_job_profile")
        })?;
    let runtime_job_kind: RuntimeKind = serde_json::from_value(
        event
            .event
            .get("runtime_job_kind")
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("agent contract completion is missing runtime_job_kind")
            })?,
    )?;
    let attempt_facts: Vec<AgentContractAttemptFact> = serde_json::from_value(
        event
            .event
            .get("agent_contract_attempts")
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("agent contract completion is missing agent_contract_attempts")
            })?,
    )?;
    if command.command_type != WorkflowCommandType::EnqueueActivity
        || command.activity_name() != Some(activity)
    {
        anyhow::bail!("agent contract completion command does not enqueue activity '{activity}'");
    }
    let contract_value = command
        .command
        .get("agent_contract")
        .ok_or_else(|| anyhow::anyhow!("agent contract completion command lost its contract"))?;
    if contract_value != &serde_json::to_value(&pinned.contract)? {
        anyhow::bail!("agent contract completion command does not match the pinned definition");
    }
    if command.command.get("prompt").and_then(Value::as_str) != Some(pinned.prompt.as_str())
        || command
            .command
            .get("definition_hash")
            .and_then(Value::as_str)
            != Some(definition.definition_hash())
    {
        anyhow::bail!(
            "agent contract completion command lost its pinned prompt or definition hash"
        );
    }
    let input = command
        .command
        .get("agent_contract_input")
        .ok_or_else(|| anyhow::anyhow!("agent contract completion command lost its input"))?;
    validate_agent_contract_input(&pinned.contract.input_schema, input)
        .map_err(anyhow::Error::msg)?;

    let assessments = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == AGENT_CONTRACT_ASSESSMENT_ARTIFACT)
        .collect::<Vec<_>>();
    let [assessment_artifact] = assessments.as_slice() else {
        anyhow::bail!("agent contract activity must contain exactly one server assessment");
    };
    let assessment: AgentContractAssessment =
        serde_json::from_value(assessment_artifact.artifact.clone())?;
    let verdicts = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == AGENT_CONTRACT_VERDICT_ARTIFACT)
        .collect::<Vec<_>>();
    let [verdict_artifact] = verdicts.as_slice() else {
        anyhow::bail!("agent contract activity must contain exactly one raw verdict artifact");
    };
    let raw_verdict = verdict_artifact
        .artifact
        .get("verdict")
        .ok_or_else(|| anyhow::anyhow!("agent contract verdict artifact is missing verdict"))?;
    validate_agent_contract_output(&pinned.contract.output_schema, &assessment.verdict)
        .map_err(anyhow::Error::msg)?;
    let expected_contract_hash = super::remote_facts::stable_remote_fact_hash(contract_value);
    let expected_input_hash = super::remote_facts::stable_remote_fact_hash(input);
    let mut expected_primary = 1;
    let mut expected_correction = 0;
    for fact in &attempt_facts {
        if fact.primary_attempt != expected_primary
            || fact.correction_attempt != expected_correction
            || fact.primary_attempt > pinned.contract.max_primary_attempts
        {
            anyhow::bail!("agent contract completion has an invalid attempt reservation sequence");
        }
        if expected_correction == pinned.contract.max_corrections {
            expected_primary = expected_primary.saturating_add(1);
            expected_correction = 0;
        } else {
            expected_correction = expected_correction.saturating_add(1);
        }
    }
    let Some(last_attempt) = attempt_facts.last() else {
        anyhow::bail!("agent contract completion has no persisted attempt reservation");
    };
    let corrections_used = attempt_facts
        .iter()
        .filter(|fact| fact.correction_attempt > 0)
        .count() as u32;
    if assessment.schema != AGENT_CONTRACT_ASSESSMENT_SCHEMA
        || assessment.assessment_id != format!("{runtime_job_id}:agent-contract-assessment")
        || assessment.activity != activity
        || assessment.definition_hash != definition.definition_hash()
        || assessment.contract_hash != expected_contract_hash
        || assessment.input_hash != expected_input_hash
        || assessment.runtime_job_id != runtime_job_id
        || assessment.command_id != command_id
        || assessment.runtime_profile != runtime_job_profile
        || assessment.runtime_kind != runtime_job_kind
        || runtime_job_kind == RuntimeKind::RemoteHost
        || assessment.outcome
            != assessment
                .verdict
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or_default()
        || raw_verdict != &assessment.verdict
        || !pinned
            .contract
            .allowed_outcomes
            .iter()
            .any(|outcome| outcome == &assessment.outcome)
        || assessment.budget.max_primary_attempts != pinned.contract.max_primary_attempts
        || assessment.budget.max_corrections != pinned.contract.max_corrections
        || assessment.budget.primary_attempts_used != last_attempt.primary_attempt
        || assessment.budget.corrections_used != corrections_used
    {
        anyhow::bail!("agent contract assessment failed pinned-event validation");
    }
    Ok(Some(assessment.outcome))
}

/// Validates and pins the agent contracts of every activity the definition
/// references. Contracts on unreferenced activities are ignored so an
/// unrelated global activity policy cannot change this definition's identity
/// or block its compilation.
pub(super) fn resolve_referenced_agent_contracts(
    policy: &WorkflowDefinitionPolicy,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<BTreeMap<String, PinnedAgentContractActivity>> {
    let mut contracts = BTreeMap::new();
    for (state_name, state) in &policy.states {
        let Some(activity) = state.activity.as_deref() else {
            continue;
        };
        let Some(activity_policy) = activity_policies.get(activity) else {
            continue;
        };
        let Some(contract) = activity_policy.agent_contract.as_ref() else {
            continue;
        };
        contract.validate(activity).map_err(|error| {
            anyhow::anyhow!("declarative workflow definition '{}': {error}", policy.id)
        })?;
        if !activity_policy.validation.is_empty() {
            anyhow::bail!(
                "declarative workflow definition '{}' activity '{}' declares an agent_contract with tools: none but also {} validation command(s); a no-tool activity cannot run validation commands",
                policy.id,
                activity,
                activity_policy.validation.len()
            );
        }
        let prompt = activity_policy
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "declarative workflow definition '{}' activity '{}' declares an agent_contract but no prompt; the effective prompt must be pinned with the contract",
                    policy.id,
                    activity
                )
            })?;
        validate_agent_contract_routes(policy, state_name, state, activity, contract)?;
        contracts.insert(
            activity.to_string(),
            PinnedAgentContractActivity {
                prompt: prompt.to_string(),
                contract: contract.clone(),
            },
        );
    }
    Ok(contracts)
}

/// A state that runs an agent-contract activity routes exclusively through the
/// contract's outcome vocabulary: its `on_signal` keys must equal the
/// `allowed_outcomes` set exactly, and `on_success` is forbidden so an
/// unrecognized outcome can never fall through to a success route.
fn validate_agent_contract_routes(
    policy: &WorkflowDefinitionPolicy,
    state_name: &str,
    state: &DeclaredState,
    activity: &str,
    contract: &WorkflowAgentContract,
) -> anyhow::Result<()> {
    if state.on_success.is_some() {
        anyhow::bail!(
            "declarative workflow definition '{}' state '{}' runs agent_contract activity '{}' and must not declare on_success; route every outcome through on_signal",
            policy.id,
            state_name,
            activity
        );
    }
    let outcomes = contract
        .allowed_outcomes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let signals = state
        .on_signal
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let missing = outcomes.difference(&signals).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' state '{}' is missing on_signal routes for agent_contract activity '{}' outcomes: {}",
            policy.id,
            state_name,
            activity,
            missing.join(", ")
        );
    }
    let extra = signals.difference(&outcomes).cloned().collect::<Vec<_>>();
    if !extra.is_empty() {
        anyhow::bail!(
            "declarative workflow definition '{}' state '{}' declares on_signal routes outside agent_contract activity '{}' allowed outcomes: {}",
            policy.id,
            state_name,
            activity,
            extra.join(", ")
        );
    }
    Ok(())
}

/// Builds the `EnqueueActivity` command for a declarative activity. When the
/// pinned definition declares an agent contract for the activity, the resolved
/// contract and definition hash are embedded in the command payload; the
/// dispatcher copies that payload into the runtime job input, so the job
/// snapshot always carries the exact contract the instance was pinned to.
pub(crate) fn declarative_enqueue_activity_command(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
    activity: &str,
    dedupe_key: String,
) -> anyhow::Result<WorkflowCommand> {
    match definition.agent_contract(activity) {
        Some(pinned) => {
            let provenance = instance.data_provenance.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "workflow '{}' cannot pin agent contract input without data provenance",
                    instance.id
                )
            })?;
            let contract_value = serde_json::to_value(&pinned.contract)?;
            let contract_hash = super::remote_facts::stable_remote_fact_hash(&contract_value);
            Ok(WorkflowCommand::new(
                WorkflowCommandType::EnqueueActivity,
                dedupe_key,
                serde_json::json!({
                    "activity": activity,
                    "agent_contract": contract_value,
                    "prompt": pinned.prompt,
                    "definition_hash": definition.definition_hash(),
                    "agent_contract_input": {
                        "schema": pinned.contract.input_schema,
                        "subject": {
                            "kind": instance.subject.subject_type,
                            "identity": instance.subject.subject_key,
                        },
                        "facts": instance.data,
                        "provenance": provenance,
                        "contract_hash": contract_hash,
                    },
                }),
            ))
        }
        None => Ok(WorkflowCommand::enqueue_activity(activity, dedupe_key)),
    }
}

/// Validates that a persisted contract command is exactly the command derived
/// from the workflow instance's pinned declarative definition and data.
pub fn validate_declarative_agent_contract_command(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<bool> {
    let command_has_contract = command.command.get("agent_contract").is_some();
    let activity = command.activity_name().ok_or_else(|| {
        anyhow::anyhow!("agent contract command does not name an enqueue activity")
    })?;
    let definition_has_contract = definition.agent_contract(activity).is_some();
    if !command_has_contract && !definition_has_contract {
        return Ok(false);
    }
    if command_has_contract != definition_has_contract {
        anyhow::bail!("agent contract command does not match the pinned workflow definition");
    }
    if instance.definition_id != definition.policy().id
        || instance.definition_version != definition.definition_version()
        || instance.data.get("definition_hash").and_then(Value::as_str)
            != Some(definition.definition_hash())
    {
        anyhow::bail!("workflow instance does not match its pinned declarative definition");
    }
    let expected = declarative_enqueue_activity_command(
        definition,
        instance,
        activity,
        command.dedupe_key.clone(),
    )?;
    if expected != *command {
        anyhow::bail!("agent contract command does not match the pinned workflow instance");
    }
    Ok(true)
}
