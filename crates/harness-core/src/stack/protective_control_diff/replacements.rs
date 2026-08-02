use super::{
    AgentStackProtectionControl, AgentStackProtectionFailureMode, AgentStackProtectionRole,
    AgentStackProtectionScope,
};
use std::collections::{BTreeMap, BTreeSet};

mod matching;
use matching::{resolve_assignments, AssignmentResolution};

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

pub(super) struct ReplacementCandidateConflicts<'a> {
    pub(super) before_any: &'a BTreeSet<String>,
    pub(super) after_any: &'a BTreeSet<String>,
}

pub(super) struct ReplacementAssignmentIndex {
    demands: BTreeMap<String, ReplacementDemandResolution>,
}

struct ReplacementDemandResolution {
    roles: Vec<AgentStackProtectionRole>,
    coverage: RoleReplacementCoverage,
    unique_candidate_id: Option<String>,
}

struct ReplacementDemand {
    roles: Vec<AgentStackProtectionRole>,
    full_candidate_ids: BTreeSet<String>,
    per_role_candidate_ids: Vec<BTreeSet<String>>,
    same_integrity_candidate_ids: BTreeSet<String>,
    contending_candidate_ids: BTreeSet<String>,
}

impl ReplacementAssignmentIndex {
    pub(super) fn coverage(
        &self,
        before: &AgentStackProtectionControl,
        roles: &[AgentStackProtectionRole],
    ) -> Option<RoleReplacementCoverage> {
        self.demand(before, roles).map(|demand| demand.coverage)
    }

    pub(super) fn unique_candidate_id(
        &self,
        before: &AgentStackProtectionControl,
        roles: &[AgentStackProtectionRole],
    ) -> Option<&str> {
        self.demand(before, roles)
            .and_then(|demand| demand.unique_candidate_id.as_deref())
    }

    fn demand(
        &self,
        before: &AgentStackProtectionControl,
        roles: &[AgentStackProtectionRole],
    ) -> Option<&ReplacementDemandResolution> {
        self.demands
            .get(before.component.component_id().as_str())
            .filter(|demand| {
                demand.roles == roles
                    || (!matches!(demand.coverage, RoleReplacementCoverage::Missing)
                        && roles.iter().all(|role| demand.roles.contains(role)))
            })
    }
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

pub(super) fn conflicted_replacement_uncovered_roles(
    before: &AgentStackProtectionControl,
    after: &[AgentStackProtectionControl],
    after_reports: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    conflicts: ReplacementCandidateConflicts<'_>,
    conflicting_replacements: &[(&AgentStackProtectionControl, Vec<AgentStackProtectionRole>)],
    additional_conflicting_replacements: &[&AgentStackProtectionControl],
) -> Vec<AgentStackProtectionRole> {
    let mut conflicting_component_ids = conflicting_replacements
        .iter()
        .map(|(candidate, _)| candidate.component.component_id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    conflicting_component_ids.extend(
        additional_conflicting_replacements
            .iter()
            .filter(|candidate| {
                conflicts
                    .after_any
                    .contains(candidate.component.component_id().as_str())
            })
            .map(|candidate| candidate.component.component_id().as_str().to_owned()),
    );
    before
        .roles
        .iter()
        .copied()
        .filter(|role| {
            let mut probe = before.clone();
            probe.roles = vec![*role];
            let has_safe_candidate = replacement_candidates(
                after,
                &probe,
                before_by_id,
                conflicts.before_any,
                CandidateMode::Equivalent,
            )
            .into_iter()
            .any(|candidate| {
                let component_id = candidate.component.component_id().as_str();
                !conflicts.before_any.contains(component_id)
                    && !conflicts.after_any.contains(component_id)
            });
            let has_individually_equivalent_conflicted_report =
                after_reports.iter().any(|report| {
                    conflicting_component_ids.contains(report.component.component_id().as_str())
                        && equivalent_replacement(&probe, report)
                });
            !has_safe_candidate && !has_individually_equivalent_conflicted_report
        })
        .collect()
}

pub(super) fn replacement_assignments(
    before: &[AgentStackProtectionControl],
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    conflicts: ReplacementStateConflicts<'_>,
) -> ReplacementAssignmentIndex {
    let mut demands = BTreeMap::<String, ReplacementDemand>::new();
    for before_control in before {
        let component_id = before_control.component.component_id().as_str();
        if before_control.roles.is_empty()
            || (before_control.enabled == Some(false)
                && !conflicts.before_enabled.contains(component_id))
        {
            continue;
        }
        let roles = replacement_demand_roles(before_control, after_by_id, &conflicts);
        if roles.is_empty() {
            continue;
        }
        let full_candidate_ids = safe_candidate_ids(
            before_control,
            roles.clone(),
            after,
            before_by_id,
            before_conflicting_component_ids,
            conflicts.after_any,
        );
        let per_role_candidate_ids = roles
            .iter()
            .map(|role| {
                safe_candidate_ids(
                    before_control,
                    vec![*role],
                    after,
                    before_by_id,
                    before_conflicting_component_ids,
                    conflicts.after_any,
                )
            })
            .collect::<Vec<_>>();
        let same_integrity_candidate_ids = replacement_candidates(
            after,
            before_control,
            before_by_id,
            before_conflicting_component_ids,
            CandidateMode::SameIntegrity,
        )
        .into_iter()
        .map(|candidate| candidate.component.component_id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
        let contending_candidate_ids = same_integrity_candidate_ids
            .iter()
            .cloned()
            .chain(per_role_candidate_ids.iter().flatten().cloned())
            .collect();
        demands.insert(
            component_id.to_owned(),
            ReplacementDemand {
                roles,
                full_candidate_ids,
                per_role_candidate_ids,
                same_integrity_candidate_ids,
                contending_candidate_ids,
            },
        );
    }

    let candidate_edges = demands
        .iter()
        .map(|(component_id, demand)| (component_id.clone(), demand.full_candidate_ids.clone()))
        .collect();
    let assignment_resolutions = resolve_assignments(&candidate_edges);
    let partial_contention = partial_candidate_contention(&demands, &assignment_resolutions);
    let resolutions = demands
        .into_iter()
        .map(|(component_id, demand)| {
            let assignment = assignment_resolutions.get(&component_id);
            let (coverage, unique_candidate_id) = match assignment {
                Some(AssignmentResolution::Unique(candidate_id))
                    if !partial_contention
                        .get(candidate_id)
                        .is_some_and(|claimants| {
                            claimants.iter().any(|claimant| claimant != &component_id)
                        })
                        && !has_alternative_split_plan(
                            &demand.per_role_candidate_ids,
                            candidate_id,
                        ) =>
                {
                    (RoleReplacementCoverage::Unique, Some(candidate_id.clone()))
                }
                Some(AssignmentResolution::Unique(_) | AssignmentResolution::Ambiguous) => {
                    (RoleReplacementCoverage::Ambiguous, None)
                }
                Some(AssignmentResolution::Missing)
                    if demand
                        .per_role_candidate_ids
                        .iter()
                        .all(|candidates| !candidates.is_empty()) =>
                {
                    (RoleReplacementCoverage::Ambiguous, None)
                }
                Some(AssignmentResolution::Missing) | None => {
                    (RoleReplacementCoverage::Missing, None)
                }
            };
            (
                component_id,
                ReplacementDemandResolution {
                    roles: demand.roles,
                    coverage,
                    unique_candidate_id,
                },
            )
        })
        .collect();
    ReplacementAssignmentIndex {
        demands: resolutions,
    }
}

fn replacement_demand_roles(
    before: &AgentStackProtectionControl,
    after_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    conflicts: &ReplacementStateConflicts<'_>,
) -> Vec<AgentStackProtectionRole> {
    let component_id = before.component.component_id().as_str();
    let Some(after) = after_by_id.get(component_id).copied() else {
        return before.roles.clone();
    };
    let has_scope_reduction = !conflicts.before_scope.contains(component_id)
        && !conflicts.after_scope.contains(component_id)
        && scope_is_reduced(before, after);
    let has_failure_mode_reduction = !conflicts.before_failure_mode.contains(component_id)
        && !conflicts.after_failure_mode.contains(component_id)
        && failure_mode_is_reduced(before, after);
    let has_enablement_evidence_loss = !conflicts.before_enabled.contains(component_id)
        && !conflicts.after_enabled.contains(component_id)
        && matches!((before.enabled, after.enabled), (Some(true), None));
    let has_confidence_reduction = after.confidence.strength() < before.confidence.strength();
    if (after.enabled == Some(false) && !conflicts.after_enabled.contains(component_id))
        || has_enablement_evidence_loss
        || has_confidence_reduction
        || has_scope_reduction
        || has_failure_mode_reduction
    {
        before.roles.clone()
    } else {
        missing_roles(before.roles(), after.roles())
    }
}

fn safe_candidate_ids(
    before: &AgentStackProtectionControl,
    roles: Vec<AgentStackProtectionRole>,
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_conflicting_component_ids: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut probe = before.clone();
    probe.roles = roles;
    replacement_candidates(
        after,
        &probe,
        before_by_id,
        before_conflicting_component_ids,
        CandidateMode::Equivalent,
    )
    .into_iter()
    .map(|candidate| candidate.component.component_id().as_str())
    .filter(|component_id| {
        !before_conflicting_component_ids.contains(*component_id)
            && !after_conflicting_component_ids.contains(*component_id)
    })
    .map(str::to_owned)
    .collect()
}

fn partial_candidate_contention(
    demands: &BTreeMap<String, ReplacementDemand>,
    assignment_resolutions: &BTreeMap<String, AssignmentResolution>,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut contention = BTreeMap::<String, BTreeSet<String>>::new();
    for (component_id, demand) in demands {
        let unique_full_candidate = match assignment_resolutions.get(component_id) {
            Some(AssignmentResolution::Unique(candidate_id)) => Some(candidate_id.as_str()),
            Some(AssignmentResolution::Ambiguous | AssignmentResolution::Missing) | None => None,
        };
        let has_complete_role_plan = unique_full_candidate.map_or_else(
            || {
                demand
                    .per_role_candidate_ids
                    .iter()
                    .all(|candidates| !candidates.is_empty())
            },
            |candidate_id| has_alternative_split_plan(&demand.per_role_candidate_ids, candidate_id),
        );
        for candidate_id in demand
            .contending_candidate_ids
            .iter()
            .filter(|candidate_id| {
                !demand.full_candidate_ids.contains(*candidate_id)
                    && (has_complete_role_plan
                        || (unique_full_candidate.is_none()
                            && demand.same_integrity_candidate_ids.contains(*candidate_id)))
            })
        {
            contention
                .entry(candidate_id.clone())
                .or_default()
                .insert(component_id.clone());
        }
    }
    contention
}

fn has_alternative_split_plan(
    per_role_candidate_ids: &[BTreeSet<String>],
    excluded_candidate_id: &str,
) -> bool {
    let alternatives = per_role_candidate_ids
        .iter()
        .map(|candidates| {
            candidates
                .iter()
                .filter(|candidate| candidate.as_str() != excluded_candidate_id)
                .cloned()
                .collect::<BTreeSet<_>>()
        })
        .collect::<Vec<_>>();
    if alternatives.iter().any(BTreeSet::is_empty) {
        return false;
    }
    let mut common = alternatives[0].clone();
    for candidates in &alternatives[1..] {
        common.retain(|candidate| candidates.contains(candidate));
    }
    common.is_empty() && alternatives.iter().flatten().collect::<BTreeSet<_>>().len() > 1
}

pub(super) fn classify_role_replacement(
    before: &AgentStackProtectionControl,
    roles: Vec<AgentStackProtectionRole>,
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_conflicting_component_ids: &BTreeSet<String>,
    replacement_assignments: &ReplacementAssignmentIndex,
) -> RoleReplacementCoverage {
    if let Some(coverage) = replacement_assignments.coverage(before, &roles) {
        return coverage;
    }
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
    if analysis.replacement_ids.len() == 1 {
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
    replacement_assignments: &ReplacementAssignmentIndex,
) -> Vec<AgentStackProtectionRole> {
    if let Some(coverage) = replacement_assignments.coverage(before, &roles) {
        return match coverage {
            RoleReplacementCoverage::Ambiguous => roles,
            RoleReplacementCoverage::Unique | RoleReplacementCoverage::Missing => Vec::new(),
        };
    }
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
            safe_candidate_ids.len() > 1
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

pub(super) fn equivalent_replacement(
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
