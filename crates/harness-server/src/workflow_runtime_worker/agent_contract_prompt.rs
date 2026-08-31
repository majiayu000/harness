//! Model-visible prompt construction for pinned agent contracts.

use harness_core::config::workflow::validate_agent_contract_input;
use serde_json::Value;

use super::agent_contract_enforcement::PinnedJobAgentContract;

/// Builds the only model-visible input for a contract attempt. The static
/// pinned instruction and immutable input envelope are serialized into one
/// prompt; no workflow document, repository state, or live request is read.
pub(super) fn contract_attempt_prompt(pinned: &PinnedJobAgentContract) -> anyhow::Result<String> {
    validate_contract_input(pinned)?;
    Ok(format!(
        "{}\n\nAgent contract input (JSON):\n{}",
        pinned.prompt,
        serde_json::to_string_pretty(&pinned.input)?
    ))
}

fn validate_contract_input(pinned: &PinnedJobAgentContract) -> anyhow::Result<()> {
    validate_agent_contract_input(&pinned.contract.input_schema, &pinned.input)
        .map_err(anyhow::Error::msg)?;
    let contract_hash = pinned
        .input
        .get("contract_hash")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("validated input is missing a string contract_hash"))?;
    let contract_value = serde_json::to_value(&pinned.contract)?;
    let expected_hash = harness_workflow::runtime::stable_remote_fact_hash(&contract_value);
    if contract_hash != expected_hash {
        anyhow::bail!(
            "agent contract input hash `{contract_hash}` does not match pinned contract hash `{expected_hash}`"
        );
    }
    Ok(())
}
