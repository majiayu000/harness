use super::support::{
    invalid_agent_output_blocked_decision, runtime_completion_evidence, signal_count,
};
use crate::runtime::completion_evidence::{
    completion_evidence_enforced, server_validation_digest_artifact,
    server_validation_digest_passed, EVIDENCE_SERVER_VALIDATION_DIGEST, WAIVER_SUMMARY,
};
use crate::runtime::model::{
    ActivityResult, WorkflowDecision, WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::quality_gate::{
    QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
};
use serde_json::Value;

pub(super) fn quality_gate_success_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if !quality_gate_activity_matches(instance, result) {
        return None;
    }

    if quality_gate_success_contract_error(result).is_none() {
        return Some(
            WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "quality_passed",
                "passed",
                "quality gate activity completed successfully with passing evidence",
            )
            .with_evidence(server_validation_digest_evidence(result))
            .with_evidence(runtime_completion_evidence(event, result))
            .high_confidence(),
        );
    }

    let reason = quality_gate_success_contract_error(result)?;
    Some(invalid_agent_output_blocked_decision(
        instance, event, result, reason,
    ))
}

pub(super) fn parent_quality_gate_pass_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        super::GITHUB_ISSUE_PR_DEFINITION_ID,
        "quality_gate_pending",
        QUALITY_GATE_ACTIVITY,
    ) {
        return None;
    }

    if let Some(reason) = quality_gate_success_contract_error(result) {
        return Some(invalid_agent_output_blocked_decision(
            instance, event, result, reason,
        ));
    }

    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "quality_gate_passed",
            "ready_to_merge",
            "quality gate passed; PR is ready to merge",
        )
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

pub(super) fn quality_gate_activity_matches(
    instance: &WorkflowInstance,
    result: &ActivityResult,
) -> bool {
    (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) == (
        QUALITY_GATE_DEFINITION_ID,
        "checking",
        QUALITY_GATE_ACTIVITY,
    )
}

pub(super) fn quality_gate_success_contract_error(result: &ActivityResult) -> Option<&'static str> {
    let passed = signal_count(result, QUALITY_PASSED_SIGNAL);
    let failed = signal_count(result, QUALITY_FAILED_SIGNAL);
    let blocked = signal_count(result, QUALITY_BLOCKED_SIGNAL);
    let status_count = passed + failed + blocked;
    if status_count == 0 {
        Some("run_quality_gate succeeded without a quality status signal")
    } else if passed == 0 {
        Some("run_quality_gate succeeded without a QualityPassed signal")
    } else if passed > 1 || failed > 0 || blocked > 0 {
        Some("run_quality_gate succeeded with ambiguous quality status signals")
    } else if !quality_gate_has_validation_evidence(result) {
        Some("run_quality_gate succeeded without validation evidence")
    } else if completion_evidence_enforced(result)
        && server_validation_digest_artifact(result).is_none()
    {
        // GH-1766 B-003: an agent QualityPassed claim without a server-side
        // validation re-run does not satisfy the gate.
        Some("run_quality_gate succeeded without a server validation digest; the server must re-execute the validation commands itself")
    } else if completion_evidence_enforced(result) && !server_validation_digest_passed(result) {
        Some("run_quality_gate claimed QualityPassed but the server validation digest records failing or unstarted commands")
    } else {
        None
    }
}

/// Decision evidence for the required `server_validation_digest` class:
/// either a summary of the server-authored digest or a recorded waiver when
/// the operator kill switch is active.
fn server_validation_digest_evidence(result: &ActivityResult) -> WorkflowEvidence {
    match server_validation_digest_artifact(result) {
        Some(digest) => {
            let commands = digest
                .get("commands")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            WorkflowEvidence::new(
                EVIDENCE_SERVER_VALIDATION_DIGEST,
                format!("server executed {commands} validation command(s), all exit 0"),
            )
        }
        None => WorkflowEvidence::new(EVIDENCE_SERVER_VALIDATION_DIGEST, WAIVER_SUMMARY),
    }
}

fn quality_gate_has_validation_evidence(result: &ActivityResult) -> bool {
    result
        .validation
        .iter()
        .any(|record| !record.command.trim().is_empty())
        || result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "validation_report")
}
