//! Shared entry point for validating a state-changing workflow decision.
//!
//! `DecisionValidator` enforces the transition allowlist, terminal-reopen
//! denial, lease ownership, and required commands/evidence. Historically only
//! the runtime-completion and operator-recovery paths consulted it, so the
//! remaining transition writers could persist a caller-supplied instance that
//! the definition never allowed (GH-1784). Every state-changing write goes
//! through `validate_transition` instead of calling the validator ad hoc.

use chrono::{DateTime, Utc};

use super::runtime_completion::validator_for_instance;
use crate::runtime::model::{WorkflowDecision, WorkflowInstance};
use crate::runtime::validator::ValidationContext;

/// Result of validating a decision against the definition it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TransitionValidation {
    Accepted,
    /// The decision must not be persisted. Carries a non-secret reason
    /// suitable for a rejected decision record.
    Rejected(String),
}

/// Validate `decision` against the validator for `current`'s definition.
///
/// Fails closed: an unresolvable definition pin or an unregistered definition
/// is a rejection, not a pass. `current` must be the instance as loaded under
/// the row lock, before the transition is applied.
pub(super) fn validate_transition(
    current: &WorkflowInstance,
    decision: &WorkflowDecision,
    source: &str,
    now: DateTime<Utc>,
) -> TransitionValidation {
    match validator_for_instance(current) {
        Ok(Some(validator)) => {
            match validator.validate(current, decision, &ValidationContext::new(source, now)) {
                Ok(()) => TransitionValidation::Accepted,
                Err(error) => TransitionValidation::Rejected(error.to_string()),
            }
        }
        Ok(None) => TransitionValidation::Rejected(format!(
            "unknown workflow definition `{}` for decision `{}`",
            current.definition_id, decision.decision
        )),
        Err(error) => TransitionValidation::Rejected(format!(
            "workflow definition could not be resolved for decision `{}`: {error}",
            decision.decision
        )),
    }
}
