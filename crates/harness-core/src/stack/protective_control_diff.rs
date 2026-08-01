use super::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentKind, AgentStackSourceScope,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

mod replacements;
use replacements::{
    analyze_role_replacements, replacement_candidates, replacement_use_counts, CandidateMode,
};
macro_rules! closed_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub const fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
        }
    };
}
#[rustfmt::skip]
closed_enum!(AgentStackProtectionRole {
    Policy => "policy",
    Hook => "hook",
    Validation => "validation",
    Sandboxing => "sandboxing",
    Check => "check",
});
#[rustfmt::skip]
closed_enum!(AgentStackProtectionDiffKind {
    Removed => "removed",
    Disabled => "disabled",
    ScopeReduced => "scope_reduced",
    FailOpen => "fail_open",
    AmbiguousReviewEvidence => "ambiguous_review_evidence",
});
#[rustfmt::skip]
closed_enum!(AgentStackProtectionConfidence {
    Low => "low",
    Medium => "medium",
    High => "high",
});
#[rustfmt::skip]
closed_enum!(AgentStackProtectionFailureMode {
    FailOpen => "fail_open",
    FailClosed => "fail_closed",
});
#[rustfmt::skip]
closed_enum!(AgentStackProtectionScope {
    Advisory => "advisory",
    Partial => "partial",
    Required => "required",
    Comprehensive => "comprehensive",
});
#[rustfmt::skip]
closed_enum!(AgentStackProtectionControlReason {
    RemovedWithoutEquivalent => "removed_without_equivalent",
    ExplicitlyDisabled => "explicitly_disabled",
    RoleSetReduced => "role_set_reduced",
    ScopeLevelReduced => "scope_level_reduced",
    FailureModeRelaxed => "failure_mode_relaxed",
    PossibleRename => "possible_rename",
    AmbiguousReplacement => "ambiguous_replacement",
    ConfidenceReduced => "confidence_reduced",
    EnablementEvidenceLost => "enablement_evidence_lost",
    ConflictingDuplicateReport => "conflicting_duplicate_report",
});
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackProtectionControlError {
    #[error("the protection role list contains a duplicate")]
    DuplicateRole,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStackProtectionControl {
    component: AgentStackComponent,
    roles: Vec<AgentStackProtectionRole>,
    enabled: Option<bool>,
    scope: Option<AgentStackProtectionScope>,
    failure_mode: Option<AgentStackProtectionFailureMode>,
    confidence: AgentStackProtectionConfidence,
}
#[rustfmt::skip]
impl AgentStackProtectionControl {
    pub fn new(
        component: AgentStackComponent,
        roles: impl IntoIterator<Item = AgentStackProtectionRole>,
        confidence: AgentStackProtectionConfidence,
    ) -> Result<Self, AgentStackProtectionControlError> {
        let roles = sorted_roles(roles)?;
        Ok(Self { component, roles, enabled: None, scope: None, failure_mode: None, confidence })
    }
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }
    pub fn with_scope(mut self, scope: AgentStackProtectionScope) -> Self {
        self.scope = Some(scope);
        self
    }
    pub fn with_failure_mode(mut self, failure_mode: AgentStackProtectionFailureMode) -> Self {
        self.failure_mode = Some(failure_mode);
        self
    }
    pub fn component(&self) -> &AgentStackComponent { &self.component }
    pub fn roles(&self) -> &[AgentStackProtectionRole] { &self.roles }
    pub fn enabled(&self) -> Option<bool> { self.enabled }
    pub fn scope(&self) -> Option<AgentStackProtectionScope> { self.scope }
    pub fn failure_mode(&self) -> Option<AgentStackProtectionFailureMode> { self.failure_mode }
    pub fn confidence(&self) -> AgentStackProtectionConfidence { self.confidence }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackProtectionControlEvidence {
    component_id: String,
    kind: AgentStackComponentKind,
    source_scope: AgentStackSourceScope,
    source_locator: String,
    integrity: Option<String>,
    capabilities: Vec<AgentStackCapability>,
    roles: Vec<AgentStackProtectionRole>,
    enabled: Option<bool>,
    scope: Option<AgentStackProtectionScope>,
    failure_mode: Option<AgentStackProtectionFailureMode>,
    confidence: AgentStackProtectionConfidence,
}
#[rustfmt::skip]
impl AgentStackProtectionControlEvidence {
    fn from_control(control: &AgentStackProtectionControl) -> Self {
        Self {
            component_id: control.component.component_id().as_str().to_owned(),
            kind: control.component.kind(),
            source_scope: control.component.source().scope(),
            source_locator: control.component.source().locator().as_str().to_owned(),
            integrity: control.component.integrity().map(|digest| digest.as_str().to_owned()),
            capabilities: control.component.capabilities().to_vec(),
            roles: control.roles.clone(),
            enabled: control.enabled,
            scope: control.scope,
            failure_mode: control.failure_mode,
            confidence: control.confidence,
        }
    }
    pub fn component_id(&self) -> &str { &self.component_id }
    pub fn kind(&self) -> AgentStackComponentKind { self.kind }
    pub fn source_scope(&self) -> AgentStackSourceScope { self.source_scope }
    pub fn source_locator(&self) -> &str { &self.source_locator }
    pub fn integrity(&self) -> Option<&str> { self.integrity.as_deref() }
    pub fn capabilities(&self) -> &[AgentStackCapability] { &self.capabilities }
    pub fn roles(&self) -> &[AgentStackProtectionRole] { &self.roles }
    pub fn enabled(&self) -> Option<bool> { self.enabled }
    pub fn scope(&self) -> Option<AgentStackProtectionScope> { self.scope }
    pub fn failure_mode(&self) -> Option<AgentStackProtectionFailureMode> { self.failure_mode }
    pub fn confidence(&self) -> AgentStackProtectionConfidence { self.confidence }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentStackProtectionControlDiff {
    kind: AgentStackProtectionDiffKind,
    roles: Vec<AgentStackProtectionRole>,
    before: Option<AgentStackProtectionControlEvidence>,
    after: Option<AgentStackProtectionControlEvidence>,
    confidence: AgentStackProtectionConfidence,
    reason: AgentStackProtectionControlReason,
}

struct ComparisonInputs<'before, 'after> {
    after_controls: &'after [AgentStackProtectionControl],
    before_by_id: &'before BTreeMap<String, &'before AgentStackProtectionControl>,
    before_conflicting_component_ids: &'before BTreeSet<String>,
    after_conflicting_component_ids: &'after BTreeSet<String>,
}
#[rustfmt::skip]
impl AgentStackProtectionControlDiff {
    fn new(
        kind: AgentStackProtectionDiffKind,
        roles: Vec<AgentStackProtectionRole>,
        before: Option<&AgentStackProtectionControl>,
        after: Option<&AgentStackProtectionControl>,
        confidence: AgentStackProtectionConfidence,
        reason: AgentStackProtectionControlReason,
    ) -> Self {
        Self {
            kind,
            roles,
            before: before.map(AgentStackProtectionControlEvidence::from_control),
            after: after.map(AgentStackProtectionControlEvidence::from_control),
            confidence,
            reason,
        }
    }
    pub fn kind(&self) -> AgentStackProtectionDiffKind { self.kind }
    pub fn roles(&self) -> &[AgentStackProtectionRole] { &self.roles }
    pub fn before(&self) -> Option<&AgentStackProtectionControlEvidence> { self.before.as_ref() }
    pub fn after(&self) -> Option<&AgentStackProtectionControlEvidence> { self.after.as_ref() }
    pub fn confidence(&self) -> AgentStackProtectionConfidence { self.confidence }
    pub fn reason(&self) -> AgentStackProtectionControlReason { self.reason }
}
pub fn protective_control_diff(
    before: &[AgentStackProtectionControl],
    after: &[AgentStackProtectionControl],
) -> Vec<AgentStackProtectionControlDiff> {
    let before_controls = aggregate_controls(before);
    let before_by_id = by_component_id(&before_controls.controls);
    let after_controls = aggregate_controls(after);
    let after_by_id = by_component_id(&after_controls.controls);
    let replacement_use_counts = replacement_use_counts(
        &before_controls.controls,
        &after_controls.controls,
        &before_by_id,
        &before_controls.conflicting_component_ids,
        &after_by_id,
    );
    let comparison_inputs = ComparisonInputs {
        after_controls: &after_controls.controls,
        before_by_id: &before_by_id,
        before_conflicting_component_ids: &before_controls.conflicting_component_ids,
        after_conflicting_component_ids: &after_controls.conflicting_component_ids,
    };
    let mut facts = Vec::new();
    for before_control in &before_controls.controls {
        let component_id = before_control.component.component_id().as_str();
        let has_before_conflict = before_controls
            .conflicting_component_ids
            .contains(component_id);
        if before_control.roles.is_empty()
            || (before_control.enabled == Some(false) && !has_before_conflict)
        {
            continue;
        }
        match after_by_id.get(component_id).copied() {
            Some(after_control) => compare_existing(
                before_control,
                after_control,
                &comparison_inputs,
                has_before_conflict
                    || after_controls
                        .conflicting_component_ids
                        .contains(component_id),
                &mut facts,
            ),
            None if has_before_conflict => {
                push_conflicting_duplicate_fact(before_control, None, &mut facts);
            }
            None => compare_removed(
                before_control,
                &after_controls.controls,
                &before_by_id,
                &before_controls.conflicting_component_ids,
                &after_controls.conflicting_component_ids,
                &replacement_use_counts,
                &mut facts,
            ),
        }
    }
    facts.sort_by(|left, right| {
        fact_sort_key(left)
            .cmp(&fact_sort_key(right))
            .then_with(|| left.reason.as_str().cmp(right.reason.as_str()))
    });
    facts
}
fn compare_existing(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
    inputs: &ComparisonInputs<'_, '_>,
    has_conflicting_duplicate_state: bool,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let confidence = min_confidence(before.confidence, after.confidence);
    if after.confidence.strength() < before.confidence.strength() {
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
        (Some(true), None) => push_existing_fact(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            before,
            after,
            confidence,
            AgentStackProtectionControlReason::EnablementEvidenceLost,
            facts,
        ),
        (left, Some(false)) if left != Some(false) => push_existing_fact(
            AgentStackProtectionDiffKind::Disabled,
            before.roles.clone(),
            before,
            after,
            confidence,
            AgentStackProtectionControlReason::ExplicitlyDisabled,
            facts,
        ),
        _ => {}
    }
    let replacements = analyze_role_replacements(
        before,
        missing_roles(before.roles(), after.roles()),
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
    if has_conflicting_duplicate_state {
        push_conflicting_duplicate_fact(before, Some(after), facts);
    }
}
fn compare_removed(
    before: &AgentStackProtectionControl,
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_conflicting_component_ids: &BTreeSet<String>,
    replacement_use_counts: &BTreeMap<String, usize>,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let mut same_integrity = replacement_candidates(
        after,
        before,
        before_by_id,
        before_conflicting_component_ids,
        CandidateMode::SameIntegrity,
    );
    if same_integrity.len() == 1 {
        let candidate = same_integrity.remove(0);
        let component_id = candidate.component.component_id().as_str();
        if after_conflicting_component_ids.contains(component_id) {
            let replacements = analyze_role_replacements(
                before,
                before.roles.clone(),
                after,
                before_by_id,
                before_conflicting_component_ids,
                after_conflicting_component_ids,
            );
            let mut uncovered_roles = replacements.uncovered_roles;
            let mut conflicting_replacements = replacements.conflicting_replacements;
            let overlapping_roles = before
                .roles
                .iter()
                .copied()
                .filter(|role| candidate.roles.contains(role))
                .collect::<Vec<_>>();
            uncovered_roles.retain(|role| !overlapping_roles.contains(role));
            if let Some((_, roles)) =
                conflicting_replacements
                    .iter_mut()
                    .find(|(conflicting, _)| {
                        conflicting.component.component_id().as_str() == component_id
                    })
            {
                *roles = merged_roles(roles, &overlapping_roles);
            } else {
                conflicting_replacements.push((candidate, overlapping_roles));
            }
            for (conflicting, roles) in conflicting_replacements {
                push_conflicting_duplicate_fact_with_roles(before, Some(conflicting), roles, facts);
            }
            if !uncovered_roles.is_empty() {
                facts.push(AgentStackProtectionControlDiff::new(
                    AgentStackProtectionDiffKind::Removed,
                    uncovered_roles,
                    Some(before),
                    None,
                    before.confidence,
                    AgentStackProtectionControlReason::RemovedWithoutEquivalent,
                ));
            }
            return;
        }
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            Some(candidate),
            rename_confidence(before, candidate),
            AgentStackProtectionControlReason::PossibleRename,
        ));
        compare_existing(
            before,
            candidate,
            &ComparisonInputs {
                after_controls: after,
                before_by_id,
                before_conflicting_component_ids,
                after_conflicting_component_ids,
            },
            false,
            facts,
        );
        return;
    }
    if !same_integrity.is_empty() {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            None,
            min_confidence(before.confidence, AgentStackProtectionConfidence::Medium),
            AgentStackProtectionControlReason::PossibleRename,
        ));
        return;
    }
    let equivalent = replacement_candidates(
        after,
        before,
        before_by_id,
        before_conflicting_component_ids,
        CandidateMode::Equivalent,
    );
    if equivalent.len() == 1 {
        let candidate = equivalent[0];
        let component_id = candidate.component.component_id().as_str();
        let has_candidate_conflict = before_conflicting_component_ids.contains(component_id)
            || after_conflicting_component_ids.contains(component_id);
        let use_count = replacement_use_counts
            .get(component_id)
            .copied()
            .unwrap_or(0);
        if use_count == 1 && !has_candidate_conflict {
            return;
        }
        let reason = if has_candidate_conflict {
            AgentStackProtectionControlReason::ConflictingDuplicateReport
        } else {
            AgentStackProtectionControlReason::AmbiguousReplacement
        };
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            Some(candidate),
            min_confidence(before.confidence, candidate.confidence),
            reason,
        ));
        return;
    }
    if let Some(candidate) = equivalent.first() {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            Some(candidate),
            AgentStackProtectionConfidence::Low,
            AgentStackProtectionControlReason::AmbiguousReplacement,
        ));
        return;
    }
    let weak_equivalent = replacement_candidates(
        after,
        before,
        before_by_id,
        before_conflicting_component_ids,
        CandidateMode::WeakEquivalent,
    );
    if let Some(candidate) = weak_equivalent.first() {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            Some(candidate),
            min_confidence(before.confidence, candidate.confidence),
            AgentStackProtectionControlReason::AmbiguousReplacement,
        ));
        return;
    }
    let replacements = analyze_role_replacements(
        before,
        before.roles.clone(),
        after,
        before_by_id,
        before_conflicting_component_ids,
        after_conflicting_component_ids,
    );
    let has_conflicting_replacements = !replacements.conflicting_replacements.is_empty();
    for (candidate, conflicting_roles) in replacements.conflicting_replacements {
        push_conflicting_duplicate_fact_with_roles(
            before,
            Some(candidate),
            conflicting_roles,
            facts,
        );
    }
    if !replacements.uncovered_roles.is_empty() {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::Removed,
            replacements.uncovered_roles,
            Some(before),
            None,
            before.confidence,
            AgentStackProtectionControlReason::RemovedWithoutEquivalent,
        ));
        return;
    }
    if has_conflicting_replacements {
        return;
    }
    if replacements.replacement_ids.len() > 1 {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            None,
            AgentStackProtectionConfidence::Low,
            AgentStackProtectionControlReason::AmbiguousReplacement,
        ));
        return;
    }
    facts.push(AgentStackProtectionControlDiff::new(
        AgentStackProtectionDiffKind::Removed,
        before.roles.clone(),
        Some(before),
        None,
        before.confidence,
        AgentStackProtectionControlReason::RemovedWithoutEquivalent,
    ));
}
#[rustfmt::skip]
fn push_existing_fact(kind: AgentStackProtectionDiffKind, roles: Vec<AgentStackProtectionRole>, before: &AgentStackProtectionControl, after: &AgentStackProtectionControl, confidence: AgentStackProtectionConfidence, reason: AgentStackProtectionControlReason, facts: &mut Vec<AgentStackProtectionControlDiff>) {
    facts.push(AgentStackProtectionControlDiff::new(kind, roles, Some(before), Some(after), confidence, reason));
}
fn by_component_id(
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
struct AggregatedControls {
    controls: Vec<AgentStackProtectionControl>,
    conflicting_component_ids: BTreeSet<String>,
}
fn aggregate_controls(controls: &[AgentStackProtectionControl]) -> AggregatedControls {
    let mut by_id = BTreeMap::<String, AgentStackProtectionControl>::new();
    let mut conflicting_component_ids = BTreeSet::new();
    for control in controls {
        let component_id = control.component.component_id().as_str().to_owned();
        if let Some(existing) = by_id.get_mut(&component_id) {
            if duplicate_state_conflicts(existing, control) {
                conflicting_component_ids.insert(component_id);
            }
            merge_control(existing, control);
        } else {
            by_id.insert(component_id, control.clone());
        }
    }
    AggregatedControls {
        controls: by_id.into_values().collect(),
        conflicting_component_ids,
    }
}
fn duplicate_state_conflicts(
    left: &AgentStackProtectionControl,
    right: &AgentStackProtectionControl,
) -> bool {
    conflicting_values(left.enabled, right.enabled)
        || conflicting_values(left.scope, right.scope)
        || conflicting_values(left.failure_mode, right.failure_mode)
        || conflicting_values(left.component.integrity(), right.component.integrity())
}
fn conflicting_values<T: Eq>(left: Option<T>, right: Option<T>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}
fn merge_control(
    existing: &mut AgentStackProtectionControl,
    control: &AgentStackProtectionControl,
) {
    if conflicting_values(
        existing.component.integrity(),
        control.component.integrity(),
    ) {
        existing.component = existing.component.clone().with_integrity(None);
    }
    existing.roles = merged_roles(existing.roles(), control.roles());
    existing.enabled = merged_after_enabled(existing.enabled, control.enabled);
    existing.scope = stronger_scope(existing.scope, control.scope);
    existing.failure_mode = stronger_failure_mode(existing.failure_mode, control.failure_mode);
    existing.confidence = min_confidence(existing.confidence, control.confidence);
}
fn push_conflicting_duplicate_fact(
    before: &AgentStackProtectionControl,
    after: Option<&AgentStackProtectionControl>,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    push_conflicting_duplicate_fact_with_roles(before, after, before.roles.clone(), facts);
}
fn push_conflicting_duplicate_fact_with_roles(
    before: &AgentStackProtectionControl,
    after: Option<&AgentStackProtectionControl>,
    roles: Vec<AgentStackProtectionRole>,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let confidence = after
        .map(|after| min_confidence(before.confidence, after.confidence))
        .unwrap_or(before.confidence);
    facts.push(AgentStackProtectionControlDiff::new(
        AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
        roles,
        Some(before),
        after,
        confidence,
        AgentStackProtectionControlReason::ConflictingDuplicateReport,
    ));
}
fn sorted_roles(
    roles: impl IntoIterator<Item = AgentStackProtectionRole>,
) -> Result<Vec<AgentStackProtectionRole>, AgentStackProtectionControlError> {
    let mut seen = BTreeSet::new();
    let mut sorted = Vec::new();
    for role in roles {
        if !seen.insert(role.as_str()) {
            return Err(AgentStackProtectionControlError::DuplicateRole);
        }
        sorted.push(role);
    }
    sorted.sort_by_key(AgentStackProtectionRole::as_str);
    Ok(sorted)
}
fn merged_roles(
    left: &[AgentStackProtectionRole],
    right: &[AgentStackProtectionRole],
) -> Vec<AgentStackProtectionRole> {
    let mut roles = left.to_vec();
    for role in right {
        if !roles.contains(role) {
            roles.push(*role);
        }
    }
    roles.sort_by_key(AgentStackProtectionRole::as_str);
    roles
}
fn merged_after_enabled(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), _) | (_, Some(true)) => Some(true),
        _ => None,
    }
}
fn stronger_scope(
    left: Option<AgentStackProtectionScope>,
    right: Option<AgentStackProtectionScope>,
) -> Option<AgentStackProtectionScope> {
    match (left, right) {
        (Some(left), Some(right)) if right.strength() > left.strength() => Some(right),
        (Some(scope), _) | (None, Some(scope)) => Some(scope),
        (None, None) => None,
    }
}
fn stronger_failure_mode(
    left: Option<AgentStackProtectionFailureMode>,
    right: Option<AgentStackProtectionFailureMode>,
) -> Option<AgentStackProtectionFailureMode> {
    if matches!(left, Some(AgentStackProtectionFailureMode::FailClosed))
        || matches!(right, Some(AgentStackProtectionFailureMode::FailClosed))
    {
        Some(AgentStackProtectionFailureMode::FailClosed)
    } else {
        left.or(right)
    }
}
fn missing_roles(
    before: &[AgentStackProtectionRole],
    after: &[AgentStackProtectionRole],
) -> Vec<AgentStackProtectionRole> {
    before
        .iter()
        .copied()
        .filter(|role| !after.contains(role))
        .collect()
}
fn min_confidence(
    left: AgentStackProtectionConfidence,
    right: AgentStackProtectionConfidence,
) -> AgentStackProtectionConfidence {
    use AgentStackProtectionConfidence as Confidence;
    match (left.strength(), right.strength()) {
        (1, _) | (_, 1) => Confidence::Low,
        (2, _) | (_, 2) => Confidence::Medium,
        _ => Confidence::High,
    }
}
fn rename_confidence(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> AgentStackProtectionConfidence {
    min_confidence(
        min_confidence(before.confidence, after.confidence),
        AgentStackProtectionConfidence::Medium,
    )
}
fn fact_sort_key(fact: &AgentStackProtectionControlDiff) -> (&str, &'static str) {
    let component_id = fact
        .before
        .as_ref()
        .or(fact.after.as_ref())
        .map(|evidence| evidence.component_id.as_str())
        .unwrap_or("");
    (component_id, fact.kind.as_str())
}
impl AgentStackProtectionScope {
    const fn strength(self) -> u8 {
        match self {
            Self::Advisory => 1,
            Self::Partial => 2,
            Self::Required => 3,
            Self::Comprehensive => 4,
        }
    }
}
impl AgentStackProtectionConfidence {
    const fn strength(self) -> u8 {
        match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}
