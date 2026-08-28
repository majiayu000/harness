//! Agent-contract resolution for declarative workflow definitions.
//!
//! An `agent_contract` on a Workflow activity declares the generic execution
//! contract for a bounded, no-tool semantic activity. This module validates
//! referenced contracts at compile time and pins the resolved contract into
//! the `EnqueueActivity` command payload so the runtime job snapshot always
//! carries the exact contract the instance was pinned to.

use super::declarative::DeclarativeWorkflowDefinition;
use super::model::{WorkflowCommand, WorkflowCommandType};
use harness_core::config::workflow::{
    DeclaredState, WorkflowActivityPolicy, WorkflowAgentContract, WorkflowDefinitionPolicy,
};
use std::collections::{BTreeMap, BTreeSet};

/// Validates and pins the agent contracts of every activity the definition
/// references. Contracts on unreferenced activities are ignored so an
/// unrelated global activity policy cannot change this definition's identity
/// or block its compilation.
pub(super) fn resolve_referenced_agent_contracts(
    policy: &WorkflowDefinitionPolicy,
    activity_policies: &BTreeMap<String, WorkflowActivityPolicy>,
) -> anyhow::Result<BTreeMap<String, WorkflowAgentContract>> {
    let mut contracts = BTreeMap::new();
    for (state_name, state) in &policy.states {
        let Some(activity) = state.activity.as_deref() else {
            continue;
        };
        let Some(contract) = activity_policies
            .get(activity)
            .and_then(|activity_policy| activity_policy.agent_contract.as_ref())
        else {
            continue;
        };
        contract.validate(activity).map_err(|error| {
            anyhow::anyhow!("declarative workflow definition '{}': {error}", policy.id)
        })?;
        validate_agent_contract_routes(policy, state_name, state, activity, contract)?;
        contracts.insert(activity.to_string(), contract.clone());
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
    activity: &str,
    dedupe_key: String,
) -> WorkflowCommand {
    match definition.agent_contract(activity) {
        Some(contract) => WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            dedupe_key,
            serde_json::json!({
                "activity": activity,
                "agent_contract": contract,
                "definition_hash": definition.definition_hash(),
            }),
        ),
        None => WorkflowCommand::enqueue_activity(activity, dedupe_key),
    }
}
