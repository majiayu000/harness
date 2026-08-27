//! Definition identity constructors for [`DecisionValidator`].

use super::{DecisionValidator, DecisionValidatorBinding, TransitionAllowlist};
use crate::runtime::WorkflowStateDefinition;

impl DecisionValidator {
    /// A validator bound to the exact declarative definition a pin resolved to.
    pub fn for_declarative_definition(
        definition_id: &str,
        definition_version: u32,
        definition_hash: &str,
        allowlist: TransitionAllowlist,
        states: Vec<WorkflowStateDefinition>,
    ) -> Self {
        let mut validator = Self::for_definition(definition_id, allowlist, states);
        validator.binding = DecisionValidatorBinding::for_declarative(
            definition_id,
            definition_version,
            definition_hash,
        );
        validator
    }

    /// A validator for a historical version that predates content-hash pins.
    pub fn for_versioned_definition(
        definition_id: &str,
        definition_version: u32,
        allowlist: TransitionAllowlist,
        states: Vec<WorkflowStateDefinition>,
    ) -> Self {
        let mut validator = Self::for_definition(definition_id, allowlist, states);
        validator.binding =
            DecisionValidatorBinding::for_versioned_definition(definition_id, definition_version);
        validator
    }

    /// The definition identity this validator was resolved from.
    pub fn binding(&self) -> &DecisionValidatorBinding {
        &self.binding
    }
}
