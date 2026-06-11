use super::support::{
    invalid_agent_output_blocked_decision, runtime_completion_evidence, signal_count,
};
use crate::runtime::model::{ActivityResult, WorkflowDecision, WorkflowEvent, WorkflowInstance};
use crate::runtime::quality_gate::{
    QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
};

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
    } else {
        None
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
