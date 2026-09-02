//! Server-authored assessment records for completed agent contracts.

use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivityStatus, RuntimeJob,
    AGENT_CONTRACT_ASSESSMENT_ARTIFACT, AGENT_CONTRACT_ASSESSMENT_SCHEMA,
    AGENT_CONTRACT_VERDICT_ARTIFACT,
};

use super::agent_contract_enforcement::PinnedJobAgentContract;

pub(super) fn attach_server_assessment(
    job: &RuntimeJob,
    pinned: &PinnedJobAgentContract,
    mut result: ActivityResult,
    primary_attempts_used: u32,
    corrections_used: u32,
) -> anyhow::Result<ActivityResult> {
    if result.status != ActivityStatus::Succeeded {
        return Ok(result);
    }
    let verdicts = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == AGENT_CONTRACT_VERDICT_ARTIFACT)
        .collect::<Vec<_>>();
    let [verdict_artifact] = verdicts.as_slice() else {
        anyhow::bail!("successful agent contract attempt must contain exactly one raw verdict");
    };
    let verdict = verdict_artifact
        .artifact
        .get("verdict")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("agent contract verdict artifact is missing verdict"))?;
    let outcome = verdict
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("agent contract verdict is missing outcome"))?
        .to_string();
    let contract_value = serde_json::to_value(&pinned.contract)?;
    let contract_hash = harness_workflow::runtime::stable_remote_fact_hash(&contract_value);
    let input_hash = harness_workflow::runtime::stable_remote_fact_hash(&pinned.input);
    result
        .artifacts
        .retain(|artifact| artifact.artifact_type != AGENT_CONTRACT_ASSESSMENT_ARTIFACT);
    let assessment = serde_json::json!({
        "schema": AGENT_CONTRACT_ASSESSMENT_SCHEMA,
        "assessment_id": format!("{}:agent-contract-assessment", job.id),
        "activity": result.activity,
        "definition_hash": pinned.definition_hash,
        "contract_hash": contract_hash,
        "input_hash": input_hash,
        "runtime_job_id": job.id,
        "command_id": job.command_id,
        "runtime_profile": job.runtime_profile,
        "runtime_kind": job.runtime_kind,
        "outcome": outcome,
        "verdict": verdict,
        "budget": {
            "max_primary_attempts": pinned.contract.max_primary_attempts,
            "max_corrections": pinned.contract.max_corrections,
            "primary_attempts_used": primary_attempts_used,
            "corrections_used": corrections_used,
        }
    });
    Ok(result.with_artifact(ActivityArtifact::new(
        AGENT_CONTRACT_ASSESSMENT_ARTIFACT,
        assessment,
    )))
}
