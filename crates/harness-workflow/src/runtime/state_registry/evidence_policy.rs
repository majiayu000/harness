//! Completion-evidence enforcement policy for the built-in workflow
//! definitions.
//!
//! Enforcement is the default: built-in transition tables declare the evidence
//! each fact-minting transition requires, and the shared decision validator
//! rejects decisions that lack it. The kill switch exists for one release, so
//! an operator can restore the previous (claim-trusting) behavior without a
//! rollback if agents have not yet caught up with the contract.

use super::{
    builtin_definitions, registry, RegisteredWorkflowDefinition, WorkflowDefinitionRegistry,
};

/// Apply the completion-evidence enforcement policy to the process-wide
/// registry's built-in definitions. Must run before the registry is frozen.
pub fn apply_builtin_evidence_enforcement(enforced: bool) -> anyhow::Result<()> {
    registry()
        .write()
        .expect("workflow definition registry lock poisoned")
        .apply_builtin_evidence_enforcement(enforced)
}

impl WorkflowDefinitionRegistry {
    /// Re-register the built-in definitions under the given completion-evidence
    /// policy. Disabling enforcement strips the declared evidence requirements
    /// while leaving every other transition rule intact.
    pub fn apply_builtin_evidence_enforcement(&mut self, enforced: bool) -> anyhow::Result<()> {
        for definition in builtin_definitions() {
            self.ensure_mutable(&definition.id)?;
            if !self.definitions.contains_key(&definition.id) {
                anyhow::bail!(
                    "built-in workflow definition '{}' is not registered; cannot apply the completion-evidence policy",
                    definition.id
                );
            }
            let definition = if enforced {
                definition
            } else {
                RegisteredWorkflowDefinition::new(
                    definition.id.clone(),
                    definition.states.clone(),
                    definition.allowlist.without_required_evidence(),
                )
            };
            Self::validate_registered_definition(&definition)?;
            self.definitions
                .insert(definition.id.clone(), std::sync::Arc::new(definition));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{builtin_definitions, WorkflowDefinitionRegistry};

    fn required_evidence_kinds(registry: &WorkflowDefinitionRegistry) -> Vec<String> {
        builtin_definitions()
            .iter()
            .filter_map(|builtin| registry.definition(&builtin.id))
            .flat_map(|definition| {
                definition
                    .allowlist
                    .rules()
                    .flat_map(|rule| rule.required_evidence.iter().cloned())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn enforced_policy_preserves_the_declared_built_in_contract() {
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        let declared = required_evidence_kinds(&registry);

        registry
            .apply_builtin_evidence_enforcement(true)
            .expect("enforcing policy applies to a registry holding the built-ins");

        assert_eq!(required_evidence_kinds(&registry), declared);
    }

    #[test]
    fn disabled_policy_strips_every_built_in_evidence_requirement() {
        let mut registry = WorkflowDefinitionRegistry::with_builtins();

        registry
            .apply_builtin_evidence_enforcement(false)
            .expect("kill switch applies to a registry holding the built-ins");

        assert!(required_evidence_kinds(&registry).is_empty());
        // Commands must survive the strip: only evidence is lifted.
        for builtin in builtin_definitions() {
            let registered = registry
                .definition(&builtin.id)
                .expect("built-in definition stays registered");
            let expected = builtin.allowlist.rules().count();
            assert_eq!(registered.allowlist.rules().count(), expected);
        }
    }

    #[test]
    fn policy_refuses_a_registry_missing_the_built_ins() {
        let mut registry = WorkflowDefinitionRegistry::new_for_tests();

        let error = registry
            .apply_builtin_evidence_enforcement(true)
            .expect_err("policy must not silently install definitions");

        assert!(error.to_string().contains("is not registered"));
    }

    #[test]
    fn frozen_registry_refuses_the_policy() {
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        registry.freeze();

        registry
            .apply_builtin_evidence_enforcement(false)
            .expect_err("a frozen registry must reject policy changes");
    }
}
