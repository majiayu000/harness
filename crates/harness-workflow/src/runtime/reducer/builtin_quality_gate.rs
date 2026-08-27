use super::support::{
    event_field_string, invalid_agent_output_blocked_decision, runtime_completion_evidence,
    signal_count,
};
use crate::runtime::completion_evidence::{
    server_validation_digest_artifact, server_validation_digest_passed,
    transition_evidence_enforced_with_registry, EVIDENCE_SERVER_VALIDATION_DIGEST,
};
use crate::runtime::model::{
    ActivityResult, WorkflowDecision, WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use crate::runtime::quality_gate::{
    QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL,
};
use crate::runtime::scope_review::enqueue_pr_scope_review;
use crate::runtime::WorkflowDefinitionRegistry;
use serde_json::Value;

pub(super) fn parent_quality_gate_head_decision(
    registry: &WorkflowDefinitionRegistry,
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
    ) || instance.definition_version == 1
        || quality_gate_success_contract_error(registry, result).is_some()
    {
        return None;
    }
    let pr_number = instance.data.get("pr_number").and_then(Value::as_u64);
    let pr_url = instance
        .data
        .get("pr_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let repo = instance
        .data
        .get("repo")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let assessed_head = instance
        .data
        .get("scope_assessed_head_oid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let snapshot = result.artifacts.iter().find_map(|artifact| {
        (artifact.artifact_type == crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT)
            .then_some(&artifact.artifact)
    });
    let snapshot_matches = snapshot.is_some_and(|snapshot| {
        snapshot.get("pr_number").and_then(Value::as_u64) == pr_number
            && snapshot
                .get("repo")
                .and_then(Value::as_str)
                .zip(repo)
                .is_some_and(|(observed, expected)| observed.eq_ignore_ascii_case(expected))
    });
    let observed_head = snapshot
        .filter(|_| snapshot_matches)
        .and_then(|snapshot| snapshot.get("head_oid"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let (Some(pr_number), Some(pr_url), Some(observed_head)) = (pr_number, pr_url, observed_head)
    else {
        return Some(invalid_agent_output_blocked_decision(
            instance,
            event,
            result,
            "quality gate passed without a current server PR snapshot bound to the scope-assessed head",
        ));
    };
    if assessed_head == Some(observed_head) {
        return None;
    }
    let issue_plan = instance
        .data
        .get("issue_plan")
        .cloned()
        .unwrap_or(Value::Null);
    let dedupe_source = event_field_string(event, "command_id").unwrap_or_else(|| event.id.clone());
    let reason = if assessed_head.is_some() {
        "server observed a new PR head after the quality gate; reassess scope before continuing"
    } else {
        "legacy quality-gate state has no model-assessed PR head; assess current scope before continuing"
    };
    Some(
        WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "reassess_pr_scope",
            "pr_scope_review",
            reason,
        )
        .with_command(enqueue_pr_scope_review(
            format!(
                "quality-gate-scope-recheck:{}:{pr_number}:{dedupe_source}",
                instance.id
            ),
            pr_number,
            pr_url,
            issue_plan,
        ))
        .with_evidence(runtime_completion_evidence(event, result))
        .high_confidence(),
    )
}

pub(super) fn quality_gate_success_decision(
    registry: &WorkflowDefinitionRegistry,
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if !quality_gate_activity_matches(instance, result) {
        return None;
    }

    if quality_gate_success_contract_error(registry, result).is_none() {
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

    let reason = quality_gate_success_contract_error(registry, result)?;
    Some(invalid_agent_output_blocked_decision(
        instance, event, result, reason,
    ))
}

pub(super) fn parent_quality_gate_pass_decision(
    registry: &WorkflowDefinitionRegistry,
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

    if let Some(reason) = quality_gate_success_contract_error(registry, result) {
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

pub(super) fn quality_gate_success_contract_error(
    registry: &WorkflowDefinitionRegistry,
    result: &ActivityResult,
) -> Option<&'static str> {
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
    } else if quality_gate_digest_enforced(registry)
        && server_validation_digest_artifact(result).is_none()
    {
        // GH-1766 B-003: an agent QualityPassed claim without a server-side
        // validation re-run does not satisfy the gate.
        Some("run_quality_gate succeeded without a server validation digest; the server must re-execute the validation commands itself")
    } else if quality_gate_digest_enforced(registry) && !server_validation_digest_passed(result) {
        Some("run_quality_gate claimed QualityPassed but the server validation digest records failing or unstarted commands")
    } else {
        None
    }
}

/// The transition table is the authority for whether `checking -> passed`
/// still demands a server validation digest (GH-1815): the deployment-global
/// kill switch strips the requirement, and this reducer gate lifts with it.
fn quality_gate_digest_enforced(registry: &WorkflowDefinitionRegistry) -> bool {
    transition_evidence_enforced_with_registry(
        registry,
        QUALITY_GATE_DEFINITION_ID,
        "checking",
        "passed",
        EVIDENCE_SERVER_VALIDATION_DIGEST,
    )
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
            WorkflowEvidence::reexecuted(
                EVIDENCE_SERVER_VALIDATION_DIGEST,
                format!("server executed {commands} validation command(s), all exit 0"),
                format!("server_validation_digest:{commands}_commands"),
                None,
            )
        }
        None => WorkflowEvidence::new(
            EVIDENCE_SERVER_VALIDATION_DIGEST,
            "enforcement_lifted_by_deployment_config",
        ),
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
