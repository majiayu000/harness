use super::{StackChangedField, StackEvidenceKind, StackPolicyError};
use harness_core::stack::capability_evidence::AgentStackCapabilityEvidenceClass;
use harness_core::stack::{
    AgentStackComponentId, AgentStackComponentKind, AgentStackSource, AgentStackSourceScope,
    AgentStackTrustLevel,
};
use std::collections::BTreeSet;

pub(super) fn ensure_non_empty(field: &str, value: &str) -> Result<(), StackPolicyError> {
    if value.trim().is_empty() {
        Err(StackPolicyError::evaluation(format!(
            "stack policy {field} cannot be empty"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn ensure_policy_non_empty(field: &str, value: &str) -> Result<(), StackPolicyError> {
    if value.trim().is_empty() {
        Err(StackPolicyError::parse(format!(
            "stack policy {field} cannot be empty"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn ensure_non_empty_list<T>(field: &str, value: &[T]) -> Result<(), StackPolicyError> {
    if value.is_empty() {
        Err(StackPolicyError::parse(format!(
            "stack policy matcher `{field}` cannot be empty"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn ensure_unique_evidence(
    label: &str,
    evidence: &[StackEvidenceKind],
) -> Result<(), StackPolicyError> {
    ensure_unique_evidence_with(label, evidence, StackPolicyError::evaluation)
}

pub(super) fn ensure_unique_policy_evidence(
    label: &str,
    evidence: &[StackEvidenceKind],
) -> Result<(), StackPolicyError> {
    ensure_unique_evidence_with(label, evidence, StackPolicyError::parse)
}

fn ensure_unique_evidence_with(
    label: &str,
    evidence: &[StackEvidenceKind],
    error: impl FnOnce(String) -> StackPolicyError + Copy,
) -> Result<(), StackPolicyError> {
    let mut seen = BTreeSet::new();
    for kind in evidence {
        if !seen.insert(*kind) {
            return Err(error(format!("duplicate {label} kind `{}`", kind.as_str())));
        }
    }
    Ok(())
}

pub(super) fn validate_capability_evidence_trust(
    evidence_class: AgentStackCapabilityEvidenceClass,
    trust_level: AgentStackTrustLevel,
) -> Result<(), StackPolicyError> {
    let valid = match evidence_class {
        AgentStackCapabilityEvidenceClass::Declared => matches!(
            trust_level,
            AgentStackTrustLevel::SelfDeclared | AgentStackTrustLevel::RepositoryObserved
        ),
        AgentStackCapabilityEvidenceClass::Granted
        | AgentStackCapabilityEvidenceClass::Observed => matches!(
            trust_level,
            AgentStackTrustLevel::RuntimeObserved | AgentStackTrustLevel::RunnerObserved
        ),
    };
    valid.then_some(()).ok_or_else(|| {
        StackPolicyError::evaluation(format!(
            "capability evidence class `{}` is incompatible with trust level `{}`",
            evidence_class.as_str(),
            trust_level.as_str()
        ))
    })
}

pub(super) fn validate_component_identity(
    component_id: &str,
    expected: Option<(AgentStackSourceScope, AgentStackComponentKind)>,
) -> Result<(), StackPolicyError> {
    let mut segments = component_id.splitn(3, ':');
    let scope = parse_closed_value(segments.next(), AgentStackSourceScope::ALL, "source scope")?;
    let kind = parse_closed_value(
        segments.next(),
        AgentStackComponentKind::ALL,
        "component kind",
    )?;
    let locator = segments.next().ok_or_else(invalid_component_id)?;
    let source = AgentStackSource::new(scope, locator).map_err(|_| invalid_component_id())?;
    let canonical = AgentStackComponentId::from_source(kind, &source);
    if canonical.as_str() != component_id || expected.is_some_and(|value| value != (scope, kind)) {
        return Err(invalid_component_id());
    }
    Ok(())
}

fn parse_closed_value<T>(
    value: Option<&str>,
    allowed: &[T],
    _label: &str,
) -> Result<T, StackPolicyError>
where
    T: ClosedValue + Copy,
{
    let value = value.ok_or_else(invalid_component_id)?;
    allowed
        .iter()
        .copied()
        .find(|candidate| candidate.wire_value() == value)
        .ok_or_else(invalid_component_id)
}

trait ClosedValue {
    fn wire_value(self) -> &'static str;
}

impl ClosedValue for AgentStackSourceScope {
    fn wire_value(self) -> &'static str {
        self.as_str()
    }
}

impl ClosedValue for AgentStackComponentKind {
    fn wire_value(self) -> &'static str {
        self.as_str()
    }
}

fn invalid_component_id() -> StackPolicyError {
    StackPolicyError::evaluation("stack policy component_id is not canonical")
}

pub(super) fn ensure_unique_fields(fields: &[StackChangedField]) -> Result<(), StackPolicyError> {
    let mut seen = BTreeSet::new();
    for field in fields {
        if !seen.insert(*field) {
            return Err(StackPolicyError::evaluation(
                "component modification facts cannot repeat changed_fields",
            ));
        }
    }
    Ok(())
}
