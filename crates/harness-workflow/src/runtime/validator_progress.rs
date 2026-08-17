use super::model::{WorkflowDecision, WorkflowInstance};
use super::state_registry::{WorkflowProgressMode, WorkflowStateDefinition};
use super::validator::{
    TransitionRule, ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
use harness_core::claim_trust::ClaimTrustLevel;
use std::collections::{BTreeMap, BTreeSet};

/// Rule metadata plus required evidence, in one call. `DecisionValidator`
/// runs the two halves separately so command-structure errors surface before
/// evidence errors; this combined form exists for tests that exercise the
/// declarative metadata contract directly.
#[cfg(test)]
pub(super) fn validate_declarative_transition_metadata(
    rule: &TransitionRule,
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    validate_declarative_transition_rule_metadata(rule, decision, context)?;
    validate_required_evidence(rule, decision)
}

pub(super) fn validate_declarative_transition_rule_metadata(
    rule: &TransitionRule,
    decision: &WorkflowDecision,
    context: &ValidationContext,
) -> Result<(), WorkflowDecisionRejection> {
    if rule.operator_recovery_only
        && (decision.observed_state != "blocked"
            || decision.decision != "operator_runtime_unblock"
            || context.actor != "workflow_runtime_operator_action")
    {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::OperatorRecoveryDenied,
            "declarative recovery transitions require an operator unblock from blocked state",
        ));
    }
    if let Some(required_command) = rule.required_command {
        if !decision
            .commands
            .iter()
            .any(|command| command.command_type == required_command)
        {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::RequiredCommandMissing,
                format!(
                    "declarative transition '{}' -> '{}' requires command {:?}",
                    decision.observed_state, decision.next_state, required_command
                ),
            ));
        }
    }
    Ok(())
}

/// Required-evidence enforcement (GH-1766). Same-state activity retries are
/// exempt: evidence requirements bind fact-minting transitions, not retries.
pub(super) fn validate_required_evidence(
    rule: &TransitionRule,
    decision: &WorkflowDecision,
) -> Result<(), WorkflowDecisionRejection> {
    let evidence_trust = validated_evidence_trust(decision)?;
    let is_activity_retry = decision.decision == "retry_failed_runtime_activity"
        && decision.observed_state == decision.next_state
        && decision.commands.len() == 1
        && decision.commands[0].command_type == super::model::WorkflowCommandType::EnqueueActivity;
    if is_activity_retry {
        return Ok(());
    }
    let evidence_kinds = evidence_trust.keys().copied().collect::<BTreeSet<_>>();
    let missing = rule
        .required_evidence
        .iter()
        .filter(|required| !evidence_kinds.contains(required.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::MissingRequiredEvidence,
            format!(
                "transition '{}' -> '{}' is missing required evidence: {}",
                decision.observed_state,
                decision.next_state,
                missing.join(", ")
            ),
        ));
    }
    let insufficient = rule
        .required_evidence
        .iter()
        .filter_map(|required| {
            let actual = evidence_trust.get(required.as_str())?;
            let minimum = rule
                .required_evidence_trust
                .get(required)
                .copied()
                .unwrap_or(ClaimTrustLevel::SelfDeclared);
            (!actual.satisfies(minimum)).then_some((required.as_str(), *actual, minimum))
        })
        .collect::<Vec<_>>();
    if !insufficient.is_empty() {
        let detail = insufficient
            .into_iter()
            .map(|(kind, actual, minimum)| {
                format!(
                    "{kind} has trust {}, requires {}",
                    actual.as_str(),
                    minimum.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::InsufficientEvidenceTrust,
            format!(
                "transition '{}' -> '{}' has insufficient evidence trust: {detail}",
                decision.observed_state, decision.next_state
            ),
        ));
    }
    Ok(())
}

fn validated_evidence_trust(
    decision: &WorkflowDecision,
) -> Result<BTreeMap<&str, ClaimTrustLevel>, WorkflowDecisionRejection> {
    let mut evidence_trust = BTreeMap::new();
    for evidence in &decision.evidence {
        evidence.validate_claim_trust().map_err(|error| {
            WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::InvalidDecisionContract,
                format!(
                    "workflow evidence `{}` has invalid claim trust: {error}",
                    evidence.kind
                ),
            )
        })?;
        evidence_trust
            .entry(evidence.kind.as_str())
            .and_modify(|trust| {
                if evidence.provenance.trust > *trust {
                    *trust = evidence.provenance.trust;
                }
            })
            .or_insert(evidence.provenance.trust);
    }
    Ok(evidence_trust)
}

pub(super) fn validate_target_progress_contract(
    state: Option<&WorkflowStateDefinition>,
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
) -> Result<(), WorkflowDecisionRejection> {
    validate_target_progress_contract_with_override(state, instance, decision, false)
}

pub(super) fn validate_target_progress_contract_with_override(
    state: Option<&WorkflowStateDefinition>,
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
    allow_missing_pinned_cancel: bool,
) -> Result<(), WorkflowDecisionRejection> {
    if state.is_none()
        && allow_missing_pinned_cancel
        && decision.decision == "cancel_declarative_submission"
        && decision.commands.len() == 1
        && decision.commands[0].command_type == super::model::WorkflowCommandType::MarkCancelled
    {
        return Ok(());
    }
    if state.is_none() {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::TransitionNotAllowed,
            format!(
                "target state '{}.{}' has no registered progress contract",
                instance.definition_id, decision.next_state
            ),
        ));
    }
    let progress_mode = state.and_then(|state| state.progress_mode);
    let has_driver = decision
        .commands
        .iter()
        .any(super::model::WorkflowCommand::requires_runtime_job);
    if progress_mode == Some(WorkflowProgressMode::CommandDriven) && !has_driver {
        return Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::ProgressDriverMissing,
            format!(
                "command-driven target state '{}.{}' requires an allowlisted runtime-job-producing command in the same decision",
                instance.definition_id, decision.next_state
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "validator_evidence_tests.rs"]
mod tests;
