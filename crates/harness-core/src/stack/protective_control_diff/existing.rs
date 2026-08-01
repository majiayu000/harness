use super::replacements::{
    analyze_role_replacements, classify_role_replacement, failure_mode_is_reduced, missing_roles,
    scope_is_reduced, shared_replacement_roles, RoleReplacementCoverage,
};
use super::*;

pub(super) fn compare_existing(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
    inputs: &ComparisonInputs<'_, '_>,
    has_conflicting_duplicate_state: bool,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let confidence = min_confidence(before.confidence, after.confidence);
    let component_id = after.component.component_id().as_str();
    let has_enabled_conflict = inputs
        .after_enabled_conflicting_component_ids
        .contains(component_id);
    let before_component_id = before.component.component_id().as_str();
    let has_before_enabled_conflict = inputs
        .before_enabled_conflicting_component_ids
        .contains(before_component_id);
    let has_scope_conflict = inputs
        .before_scope_conflicting_component_ids
        .contains(before_component_id)
        || inputs
            .after_scope_conflicting_component_ids
            .contains(component_id);
    let has_failure_mode_conflict = inputs
        .before_failure_mode_conflicting_component_ids
        .contains(before_component_id)
        || inputs
            .after_failure_mode_conflicting_component_ids
            .contains(component_id);
    let disablement_coverage =
        if before.enabled != Some(false) && after.enabled == Some(false) && !has_enabled_conflict {
            Some(classify_role_replacement(
                before,
                before.roles.clone(),
                inputs.after_controls,
                inputs.before_by_id,
                inputs.before_conflicting_component_ids,
                inputs.after_conflicting_component_ids,
                inputs.replacement_use_counts,
            ))
        } else {
            None
        };
    let disablement_has_complete_coverage = matches!(
        disablement_coverage,
        Some(RoleReplacementCoverage::Unique | RoleReplacementCoverage::Ambiguous)
    );
    let missing_roles = if has_before_enabled_conflict {
        Vec::new()
    } else {
        missing_roles(before.roles(), after.roles())
    };
    let has_scope_reduction = !has_scope_conflict && scope_is_reduced(before, after);
    let has_failure_mode_reduction =
        !has_failure_mode_conflict && failure_mode_is_reduced(before, after);
    let has_enablement_evidence_loss =
        matches!((before.enabled, after.enabled), (Some(true), None));
    let has_confidence_reduction = after.confidence.strength() < before.confidence.strength();
    let standalone_state_reduction_coverage = if after.enabled != Some(false)
        && (has_enablement_evidence_loss
            || has_confidence_reduction
            || has_scope_reduction
            || has_failure_mode_reduction)
    {
        Some(classify_role_replacement(
            before,
            before.roles.clone(),
            inputs.after_controls,
            inputs.before_by_id,
            inputs.before_conflicting_component_ids,
            inputs.after_conflicting_component_ids,
            inputs.replacement_use_counts,
        ))
    } else {
        None
    };
    let standalone_state_reduction_has_complete_coverage = matches!(
        standalone_state_reduction_coverage,
        Some(RoleReplacementCoverage::Unique | RoleReplacementCoverage::Ambiguous)
    );
    if !disablement_has_complete_coverage
        && !standalone_state_reduction_has_complete_coverage
        && has_confidence_reduction
    {
        push_existing_fact(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            before,
            after,
            confidence,
            AgentStackProtectionControlReason::ConfidenceReduced,
            facts,
        );
    }
    match (before.enabled, after.enabled) {
        (Some(true), None) => match standalone_state_reduction_coverage {
            Some(RoleReplacementCoverage::Unique | RoleReplacementCoverage::Ambiguous) => {}
            Some(RoleReplacementCoverage::Missing) | None => push_existing_fact(
                AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
                before.roles.clone(),
                before,
                after,
                confidence,
                AgentStackProtectionControlReason::EnablementEvidenceLost,
                facts,
            ),
        },
        (left, Some(false)) if left != Some(false) && !has_enabled_conflict => {
            match disablement_coverage {
                Some(RoleReplacementCoverage::Unique) => {}
                Some(RoleReplacementCoverage::Ambiguous) => push_existing_fact(
                    AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
                    before.roles.clone(),
                    before,
                    after,
                    confidence,
                    AgentStackProtectionControlReason::AmbiguousReplacement,
                    facts,
                ),
                Some(RoleReplacementCoverage::Missing) => push_existing_fact(
                    AgentStackProtectionDiffKind::Disabled,
                    before.roles.clone(),
                    before,
                    after,
                    confidence,
                    AgentStackProtectionControlReason::ExplicitlyDisabled,
                    facts,
                ),
                None => {}
            }
        }
        _ => {}
    }
    let shared_replacement_roles = shared_replacement_roles(
        before,
        missing_roles.clone(),
        inputs.after_controls,
        inputs.before_by_id,
        inputs.before_conflicting_component_ids,
        inputs.after_conflicting_component_ids,
        inputs.replacement_use_counts,
    );
    let replacements = analyze_role_replacements(
        before,
        missing_roles,
        inputs.after_controls,
        inputs.before_by_id,
        inputs.before_conflicting_component_ids,
        inputs.after_conflicting_component_ids,
    );
    for (candidate, conflicting_roles) in replacements.conflicting_replacements {
        push_conflicting_duplicate_fact_with_roles(
            before,
            Some(candidate),
            conflicting_roles,
            facts,
        );
    }
    if !replacements.uncovered_roles.is_empty() {
        push_existing_fact(
            AgentStackProtectionDiffKind::ScopeReduced,
            replacements.uncovered_roles,
            before,
            after,
            confidence,
            AgentStackProtectionControlReason::RoleSetReduced,
            facts,
        );
    }
    let standalone_state_replacement_is_ambiguous = matches!(
        standalone_state_reduction_coverage,
        Some(RoleReplacementCoverage::Ambiguous)
    );
    let disablement_replacement_is_ambiguous = matches!(
        disablement_coverage,
        Some(RoleReplacementCoverage::Ambiguous)
    );
    if !shared_replacement_roles.is_empty()
        && !disablement_replacement_is_ambiguous
        && !standalone_state_replacement_is_ambiguous
    {
        push_existing_fact(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            shared_replacement_roles,
            before,
            after,
            confidence,
            AgentStackProtectionControlReason::AmbiguousReplacement,
            facts,
        );
    }
    if standalone_state_replacement_is_ambiguous {
        push_existing_fact(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            before,
            after,
            confidence,
            AgentStackProtectionControlReason::AmbiguousReplacement,
            facts,
        );
    }
    if !disablement_has_complete_coverage
        && !standalone_state_reduction_has_complete_coverage
        && !has_scope_conflict
    {
        if before.scope.is_some() && after.scope.is_none() {
            push_existing_fact(
                AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
                before.roles.clone(),
                before,
                after,
                confidence,
                AgentStackProtectionControlReason::ScopeLevelReduced,
                facts,
            );
        } else if matches!((before.scope, after.scope), (Some(left), Some(right)) if right.strength() < left.strength())
        {
            push_existing_fact(
                AgentStackProtectionDiffKind::ScopeReduced,
                before.roles.clone(),
                before,
                after,
                confidence,
                AgentStackProtectionControlReason::ScopeLevelReduced,
                facts,
            );
        }
    }
    if !disablement_has_complete_coverage
        && !standalone_state_reduction_has_complete_coverage
        && !has_failure_mode_conflict
    {
        match (before.failure_mode, after.failure_mode) {
            (
                Some(AgentStackProtectionFailureMode::FailClosed),
                Some(AgentStackProtectionFailureMode::FailOpen),
            ) => push_existing_fact(
                AgentStackProtectionDiffKind::FailOpen,
                before.roles.clone(),
                before,
                after,
                confidence,
                AgentStackProtectionControlReason::FailureModeRelaxed,
                facts,
            ),
            (Some(AgentStackProtectionFailureMode::FailClosed), None) => push_existing_fact(
                AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
                before.roles.clone(),
                before,
                after,
                confidence,
                AgentStackProtectionControlReason::FailureModeRelaxed,
                facts,
            ),
            _ => {}
        }
    }
    if has_conflicting_duplicate_state {
        push_conflicting_duplicate_fact(before, Some(after), facts);
    }
}
