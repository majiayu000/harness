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
    let after_by_id = by_component_id(after);
    let mut facts = Vec::new();

    for before_control in before {
        if before_control.roles.is_empty() {
            continue;
        }
        let component_id = before_control.component.component_id().as_str();
        match after_by_id.get(component_id).copied() {
            Some(after_control) => compare_existing(before_control, after_control, &mut facts),
            None => compare_removed(before_control, after, &mut facts),
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
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let confidence = min_confidence(before.confidence, after.confidence);
    if before.enabled == Some(true) && after.enabled == Some(false) {
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
}

fn compare_removed(
    before: &AgentStackProtectionControl,
    after: &[AgentStackProtectionControl],
    facts: &mut Vec<AgentStackProtectionControlDiff>,
) {
    let mut same_integrity = replacement_candidates(after, before, CandidateMode::SameIntegrity);
    if let Some(candidate) = same_integrity.pop() {
        facts.push(AgentStackProtectionControlDiff::new(
            AgentStackProtectionDiffKind::AmbiguousReviewEvidence,
            before.roles.clone(),
            Some(before),
            Some(candidate),
            AgentStackProtectionConfidence::Medium,
            AgentStackProtectionControlReason::PossibleRename,
        ));
        return;
    }

    let equivalent = replacement_candidates(after, before, CandidateMode::Equivalent);
    if equivalent.len() == 1 {
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
}

fn replacement_candidates<'a>(
    after: &'a [AgentStackProtectionControl],
    before: &AgentStackProtectionControl,
    mode: CandidateMode,
) -> Vec<&'a AgentStackProtectionControl> {
    let mut candidates = after
        .iter()
        .filter(|candidate| role_overlap(before.roles(), candidate.roles()))
        .filter(|candidate| match mode {
            CandidateMode::SameIntegrity => same_integrity(before, candidate),
            CandidateMode::Equivalent => equivalent_replacement(before, candidate),
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.component.component_id().as_str());
    candidates
}

fn equivalent_replacement(
    before: &AgentStackProtectionControl,
    after: &AgentStackProtectionControl,
) -> bool {
    roles_include(after.roles(), before.roles())
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
    let mut by_id = BTreeMap::new();
    for control in controls {
        by_id
            .entry(control.component.component_id().as_str().to_owned())
            .or_insert(control);
    }
    by_id
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
