use super::{
    AgentStackCapability, AgentStackComponent, AgentStackComponentKind, AgentStackSourceScope,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
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
    ConflictingDuplicateReport => "conflicting_duplicate_report",
});
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentStackProtectionControlError {
    #[error("the protection role list contains a duplicate")]
    DuplicateRole,
}
/// Typed evidence that one Agent Stack component carries protection behavior.
///
/// A component is treated as protection-bearing only when explicit roles are
/// supplied. This prevents hook or policy filenames from becoming protection
/// claims on their own.
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
/// Emit weakening evidence for explicit protection-bearing controls.
///
/// The detector is intentionally evidence-driven: entries with no protection
/// roles are ignored, renamed controls become review evidence, and equivalent
/// replacements suppress removal facts.
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
    has_conflicting_duplicate_state: bool,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let confidence = min_confidence(before.confidence, after.confidence);
    if before.enabled != Some(false) && after.enabled == Some(false) {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::Disabled,
            before.roles.clone(),
            Some(before),
            Some(after),
            confidence,
            AgentStackProtectionControlReason::ExplicitlyDisabled,
        ));
    }
    let missing_roles = missing_roles(before.roles(), after.roles());
    if !missing_roles.is_empty() {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::ScopeReduced,
            missing_roles,
            Some(before),
            Some(after),
            confidence,
            AgentStackProtectionControlReason::RoleSetReduced,
        ));
    }
    if matches!((before.scope, after.scope), (Some(left), Some(right)) if right.strength() < left.strength())
    {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::ScopeReduced,
            before.roles.clone(),
            Some(before),
            Some(after),
            confidence,
            AgentStackProtectionControlReason::ScopeLevelReduced,
        ));
    }
    if before.failure_mode == Some(AgentStackProtectionFailureMode::FailClosed)
        && after.failure_mode == Some(AgentStackProtectionFailureMode::FailOpen)
    {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::FailOpen,
            before.roles.clone(),
            Some(before),
            Some(after),
            confidence,
            AgentStackProtectionControlReason::FailureModeRelaxed,
        ));
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
            push_conflicting_duplicate_fact(before, Some(candidate), facts);
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
        compare_existing(before, candidate, false, facts);
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
    facts.push(AgentStackProtectionControlDiff::new(
        AgentStackProtectionDiffKind::Removed,
        before.roles.clone(),
        Some(before),
        None,
        before.confidence,
        AgentStackProtectionControlReason::RemovedWithoutEquivalent,
    ));
}
#[derive(Clone, Copy)]
enum CandidateMode {
    SameIntegrity,
    Equivalent,
    WeakEquivalent,
}
fn replacement_candidates<'a>(
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
        Some(previous) => {
            previous.enabled == Some(false) || !weak_equivalent_replacement(removed, previous)
        }
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
fn replacement_use_counts(
    before: &[AgentStackProtectionControl],
    after: &[AgentStackProtectionControl],
    before_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
    before_conflicting_component_ids: &BTreeSet<String>,
    after_by_id: &BTreeMap<String, &AgentStackProtectionControl>,
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for before_control in before {
        if before_control.roles.is_empty() || before_control.enabled == Some(false) {
            continue;
        }
        if after_by_id.contains_key(before_control.component.component_id().as_str()) {
            continue;
        }
        let mut used_ids = BTreeSet::new();
        for mode in [CandidateMode::SameIntegrity, CandidateMode::Equivalent] {
            for candidate in replacement_candidates(
                after,
                before_control,
                before_by_id,
                before_conflicting_component_ids,
                mode,
            ) {
                used_ids.insert(candidate.component.component_id().as_str().to_owned());
            }
        }
        for component_id in used_ids {
            *counts.entry(component_id).or_insert(0) += 1;
        }
    }
    counts
}
fn push_conflicting_duplicate_fact(
    before: &AgentStackProtectionControl,
    after: Option<&AgentStackProtectionControl>,
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let confidence = after
        .map(|after| min_confidence(before.confidence, after.confidence))
        .unwrap_or(before.confidence);
    facts.push(AgentStackProtectionControlDiff::new(
        AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
        before.roles.clone(),
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
fn role_overlap(left: &[AgentStackProtectionRole], right: &[AgentStackProtectionRole]) -> bool {
    left.iter().any(|role| right.contains(role))
}
fn roles_include(left: &[AgentStackProtectionRole], right: &[AgentStackProtectionRole]) -> bool {
    right.iter().all(|role| left.contains(role))
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
