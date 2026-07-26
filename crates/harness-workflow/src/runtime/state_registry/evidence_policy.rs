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

/// Whether the registered definition still demands `evidence_kind` for this
/// transition.
///
/// Reducers refuse to build a claim-only terminal decision in the first place,
/// so that the agent gets a precise reason instead of a bare rejection. They
/// ask this rather than tracking enforcement separately: the transition table
/// is the single authority, so the kill switch that strips a requirement lifts
/// the reducer gate with it, and the two layers cannot drift apart.
pub fn transition_requires_evidence(
    definition_id: &str,
    from_state: &str,
    to_state: &str,
    evidence_kind: &str,
) -> bool {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .transition_requires_evidence(definition_id, from_state, to_state, evidence_kind)
}

impl WorkflowDefinitionRegistry {
    /// Whether `definition_id`'s rule for this transition declares
    /// `evidence_kind`. Unknown definitions and unknown transitions declare
    /// nothing.
    pub fn transition_requires_evidence(
        &self,
        definition_id: &str,
        from_state: &str,
        to_state: &str,
        evidence_kind: &str,
    ) -> bool {
        self.definition(definition_id).is_some_and(|definition| {
            definition
                .allowlist
                .rule_for(from_state, to_state)
                .is_some_and(|rule| rule.required_evidence.contains(evidence_kind))
        })
    }

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
    fn the_kill_switch_lifts_the_reducer_gate_with_the_transition_requirement() {
        // Reducers ask `transition_requires_evidence` rather than tracking
        // enforcement separately, so stripping the requirement must make that
        // query answer false — otherwise an operator who disabled the contract
        // would still be blocked by the reducer.
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        assert!(registry.transition_requires_evidence(
            crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID,
            "implementing",
            "done",
            crate::runtime::model::EVIDENCE_PROMPT_COMPLETION,
        ));

        registry
            .apply_builtin_evidence_enforcement(false)
            .expect("kill switch applies to a registry holding the built-ins");

        assert!(!registry.transition_requires_evidence(
            crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID,
            "implementing",
            "done",
            crate::runtime::model::EVIDENCE_PROMPT_COMPLETION,
        ));
    }

    #[test]
    fn unknown_definitions_and_transitions_declare_nothing() {
        let registry = WorkflowDefinitionRegistry::with_builtins();
        assert!(!registry.transition_requires_evidence(
            "no_such_definition",
            "implementing",
            "done",
            crate::runtime::model::EVIDENCE_PROMPT_COMPLETION,
        ));
        assert!(!registry.transition_requires_evidence(
            crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID,
            "implementing",
            "no_such_state",
            crate::runtime::model::EVIDENCE_PROMPT_COMPLETION,
        ));
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
