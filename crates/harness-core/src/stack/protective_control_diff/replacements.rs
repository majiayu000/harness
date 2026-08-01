use super::{
    AgentStackProtectionControl, AgentStackProtectionFailureMode, AgentStackProtectionRole,
    AgentStackProtectionScope,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy)]
pub(super) enum CandidateMode {
    SameIntegrity,
    Equivalent,
    WeakEquivalent,
}

#[derive(Clone, Copy)]
pub(super) enum RoleReplacementCoverage {
    Unique,
    Ambiguous,
    Missing,
}

pub(super) struct RoleReplacementAnalysis<'a> {
    pub(super) uncovered_roles: Vec<AgentStackProtectionRole>,
    pub(super) conflicting_replacements: Vec<(
        &'a AgentStackProtectionControl,
        Vec<AgentStackProtectionRole>,
    )>,
    pub(super) replacement_ids: BTreeSet<String>,
}

pub(super) struct ReplacementStateConflicts<'a> {
    pub(super) after_any: &'a BTreeSet<String>,
    pub(super) before_enabled: &'a BTreeSet<String>,
    pub(super) after_enabled: &'a BTreeSet<String>,
    pub(super) before_scope: &'a BTreeSet<String>,
    pub(super) after_scope: &'a BTreeSet<String>,
    pub(super) before_failure_mode: &'a BTreeSet<String>,
    pub(super) after_failure_mode: &'a BTreeSet<String>,
}

pub(super) fn replacement_candidates<'a>(
    after: &'a [AgentStackProtectionControl],
    before: &AgentStackProtectionControl,
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    mode: CandidateMode,
) -> Vec<&'a AgentStackProtectionControl> {
    let mut candidates = after
        .iter()
        .filter(|candidate| {
            candidate_is_new_protection(
                candidate,
                before,
                before_by_id,
                before_conflicting_component_ids,
                mode,
            )
        })
        .filter(|candidate| role_overlap(before.roles(), candidate.roles()))
        .filter(|candidate| match mode {
            CandidateMode::SameIntegrity => same_integrity(before, candidate),
            CandidateMode::Equivalent => equivalent_replacement(before, candidate),
            CandidateMode::WeakEquivalent => weak_equivalent_replacement(before, candidate),
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.component.component_id().as_str());
    candidates
}

pub(super) fn analyze_role_replacements<'a>(
    before: &AgentStackProtectionControl,
    roles: Vec<AgentStackProtectionRole>,
    after: &'a [AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_conflicting_component_ids: &BTreeSet<String>,
) -> RoleReplacementAnalysis<'a> {
    let mut uncovered = Vec::new();
    let mut conflicting =
        BTreeMap::<String, (&AgentStackProtectionControl, Vec<AgentStackProtectionRole>)>::new();
    let mut replacement_ids = BTreeSet::new();
    for role in roles {
        let mut probe = before.clone();
        probe.roles = vec![role];
        let candidates = replacement_candidates(
            after,
            &probe,
            before_by_id,
            before_conflicting_component_ids,
            CandidateMode::Equivalent,
        );
        let mut has_safe_candidate = false;
        let mut role_conflicting_candidates = Vec::new();
        for candidate in candidates {
            let component_id = candidate.component.component_id().as_str();
            if after_conflicting_component_ids.contains(component_id) {
                role_conflicting_candidates.push(candidate);
            } else {
                has_safe_candidate = true;
                replacement_ids.insert(component_id.to_owned());
            }
        }
        if !has_safe_candidate {
            if role_conflicting_candidates.is_empty() {
                uncovered.push(role);
            } else {
                for candidate in role_conflicting_candidates {
                    conflicting
                        .entry(candidate.component.component_id().as_str().to_owned())
                        .or_insert_with(|| (candidate, Vec::new()))
                        .1
                        .push(role);
                }
            }
        }
    }
    RoleReplacementAnalysis {
        uncovered_roles: uncovered,
        conflicting_replacements: conflicting.into_values().collect(),
        replacement_ids,
    }
}

pub(super) fn replacement_use_counts(
    before: &[AgentStackProtectionControl],
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    conflicts: ReplacementStateConflicts<'_>,
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for before_control in before {
        let component_id = before_control.component.component_id().as_str();
        if before_control.roles.is_empty()
            || (before_control.enabled == Some(false)
                && !conflicts.before_enabled.contains(component_id))
        {
            continue;
        }
        let Some(after_control) = after_by_id.get(component_id).copied() else {
            let mut used_ids = BTreeSet::new();
            let same_integrity = replacement_candidates(
                after,
                before_control,
                before_by_id,
                before_conflicting_component_ids,
                CandidateMode::SameIntegrity,
            );
            let equivalent = replacement_candidates(
                after,
                before_control,
                before_by_id,
                before_conflicting_component_ids,
                CandidateMode::Equivalent,
            );
            for candidate in same_integrity.iter().chain(equivalent.iter()) {
                used_ids.insert(candidate.component.component_id().as_str().to_owned());
            }
            for role in before_control.roles.iter().copied().filter(|role| {
                let mut probe = before_control.clone();
                probe.roles = vec![*role];
                !same_integrity.iter().any(|candidate| {
                    let candidate_id = candidate.component.component_id().as_str();
                    !conflicts.after_any.contains(candidate_id)
                        && equivalent_replacement(&probe, candidate)
                })
            }) {
                let mut probe = before_control.clone();
                probe.roles = vec![role];
                for candidate in replacement_candidates(
                    after,
                    &probe,
                    before_by_id,
                    before_conflicting_component_ids,
                    CandidateMode::Equivalent,
                ) {
                    used_ids.insert(candidate.component.component_id().as_str().to_owned());
                }
            }
            increment_use_counts(&mut counts, used_ids);
            continue;
        };
        let has_scope_reduction = !conflicts.before_scope.contains(component_id)
            && !conflicts.after_scope.contains(component_id)
            && scope_is_reduced(before_control, after_control);
        let has_failure_mode_reduction = !conflicts.before_failure_mode.contains(component_id)
            && !conflicts.after_failure_mode.contains(component_id)
            && failure_mode_is_reduced(before_control, after_control);
        let has_enablement_evidence_loss = !conflicts.before_enabled.contains(component_id)
            && !conflicts.after_enabled.contains(component_id)
            && matches!(
                (before_control.enabled, after_control.enabled),
                (Some(true), None)
            );
        let roles = if (after_control.enabled == Some(false)
            && !conflicts.after_enabled.contains(component_id))
            || has_enablement_evidence_loss
            || has_scope_reduction
            || has_failure_mode_reduction
        {
            before_control.roles.clone()
        } else {
            missing_roles(before_control.roles(), after_control.roles())
        };
        let mut used_ids = BTreeSet::new();
        for role in roles {
            let mut probe = before_control.clone();
            probe.roles = vec![role];
            for candidate in replacement_candidates(
                after,
                &probe,
                before_by_id,
                before_conflicting_component_ids,
                CandidateMode::Equivalent,
            ) {
                used_ids.insert(candidate.component.component_id().as_str().to_owned());
            }
        }
        increment_use_counts(&mut counts, used_ids);
    }
    counts
}

pub(super) fn classify_role_replacement(
    before: &AgentStackProtectionControl,
    roles: Vec<AgentStackProtectionRole>,
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_conflicting_component_ids: &BTreeSet<String>,
    replacement_use_counts: &BTreeMap<String, usize>,
) -> RoleReplacementCoverage {
    let analysis = analyze_role_replacements(
        before,
        roles,
        after,
        before_by_id,
        before_conflicting_component_ids,
        after_conflicting_component_ids,
    );
    if !analysis.uncovered_roles.is_empty()
        || !analysis.conflicting_replacements.is_empty()
        || analysis.replacement_ids.is_empty()
    {
        return RoleReplacementCoverage::Missing;
    }
    if analysis.replacement_ids.len() == 1
        && analysis
            .replacement_ids
            .iter()
            .all(|component_id| replacement_use_counts.get(component_id).copied() == Some(1))
    {
        RoleReplacementCoverage::Unique
    } else {
        RoleReplacementCoverage::Ambiguous
    }
}

pub(super) fn shared_replacement_roles(
    before: &AgentStackProtectionControl,
    roles: Vec<AgentStackProtectionRole>,
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_conflicting_component_ids: &BTreeSet<String>,
    replacement_use_counts: &BTreeMap<String, usize>,
) -> Vec<AgentStackProtectionRole> {
    roles
        .into_iter()
        .filter(|role| {
            let mut probe = before.clone();
            probe.roles = vec![*role];
            let safe_candidate_ids = replacement_candidates(
                after,
                &probe,
                before_by_id,
                before_conflicting_component_ids,
                CandidateMode::Equivalent,
            )
            .into_iter()
            .map(|candidate| candidate.component.component_id().as_str())
            .filter(|component_id| !after_conflicting_component_ids.contains(*component_id))
            .collect::<Vec<_>>();
            !safe_candidate_ids.is_empty()
                && safe_candidate_ids.iter().all(|component_id| {
                    replacement_use_counts
                        .get(*component_id)
                        .copied()
                        .unwrap_or(0)
                        > 1
                })
        })
        .collect()
}

pub(super) fn by_component_id(
    controls: &[AgentStackProtectionControl],
) -> BTreeMap<String, &AgentStackProtectionControl> {
    controls
        .iter()
        .map(|control| {
            (
                control.component.component_id().as_str().to_owned(),
                control,
            )
        })
        .collect()
}

pub(super) fn missing_roles(
    before: &[AgentStackProtectionRole],
    after: &[AgentStackProtectionRole],
) -> Vec<AgentStackProtectionRole> {
    before
        .iter()
        .copied()
        .filter(|role| !after.contains(role))
        .collect()
}

pub(super) fn scope_is_reduced(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> bool {
    match (before.scope, after.scope) {
        (Some(_), None) => true,
        (Some(before), Some(after)) => after.strength() < before.strength(),
        _ => false,
    }
}

pub(super) fn failure_mode_is_reduced(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> bool {
    matches!(
        (before.failure_mode, after.failure_mode),
        (
            Some(AgentStackProtectionFailureMode::FailClosed),
            Some(AgentStackProtectionFailureMode::FailOpen) | None,
        )
    )
}

fn increment_use_counts(counts: &mut BTreeMap<String, usize>, component_ids: BTreeSet<String>) {
    for component_id in component_ids {
        *counts.entry(component_id).or_insert(0) += 1;
    }
}

fn candidate_is_new_protection(
    candidate: &AgentStackProtectionControl,
    removed: &AgentStackProtectionControl,
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    mode: CandidateMode,
) -> bool {
    let component_id = candidate.component.component_id().as_str();
    match before_by_id.get(component_id).copied() {
        Some(_) if matches!(mode, CandidateMode::SameIntegrity) => false,
        Some(_) if before_conflicting_component_ids.contains(component_id) => true,
        Some(previous) => match mode {
            CandidateMode::SameIntegrity => false,
            CandidateMode::Equivalent => !equivalent_replacement(removed, previous),
            CandidateMode::WeakEquivalent => !weak_equivalent_replacement(removed, previous),
        },
        None => true,
    }
}

fn equivalent_replacement(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> bool {
    after.confidence.strength() >= before.confidence.strength()
        && weak_equivalent_replacement(before, after)
}

fn weak_equivalent_replacement(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> bool {
    after.enabled != Some(false)
        && roles_include(after.roles(), before.roles())
        && (before.enabled != Some(true) || after.enabled == Some(true))
        && scope_at_least(after.scope, before.scope)
        && (before.failure_mode != Some(AgentStackProtectionFailureMode::FailClosed)
            || after.failure_mode == Some(AgentStackProtectionFailureMode::FailClosed))
}

fn scope_at_least(
    after: Option<AgentStackProtectionScope>,
    before: Option<AgentStackProtectionScope>,
) -> bool {
    match before {
        Some(before_scope) => {
            after.is_some_and(|after_scope| after_scope.strength() >= before_scope.strength())
        }
        None => true,
    }
}

fn same_integrity(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> bool {
    matches!(
        (before.component.integrity(), after.component.integrity()),
        (Some(left), Some(right)) if left.as_str() == right.as_str()
    )
}

fn role_overlap(left: &[AgentStackProtectionRole], right: &[AgentStackProtectionRole]) -> bool {
    left.iter().any(|role| right.contains(role))
}

fn roles_include(left: &[AgentStackProtectionRole], right: &[AgentStackProtectionRole]) -> bool {
    right.iter().all(|role| left.contains(role))
}
