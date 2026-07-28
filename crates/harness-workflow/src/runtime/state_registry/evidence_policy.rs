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
        self.apply_evidence_enforcement(builtin_definitions(), enforced)
    }

    fn apply_evidence_enforcement(
        &mut self,
        definitions: impl IntoIterator<Item = RegisteredWorkflowDefinition>,
        enforced: bool,
    ) -> anyhow::Result<()> {
        let mut staged = self.clone();
        for definition in definitions {
            staged.ensure_mutable(&definition.id)?;
            if !staged.definitions.contains_key(&definition.id) {
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
            staged
                .definitions
                .insert(definition.id.clone(), std::sync::Arc::new(definition));
        }
        *self = staged;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{builtin_definitions, WorkflowDefinitionRegistry};
    use crate::runtime::{
        ValidationContext, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
        WorkflowDecisionRejectionKind, WorkflowInstance, WorkflowSubject,
    };
    use chrono::Utc;
    use serde_json::json;

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
    fn failed_policy_change_leaves_the_registry_unchanged() {
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        let definitions = builtin_definitions();
        let preserved_id = definitions[0].id.clone();
        let missing_id = definitions[1].id.clone();
        registry.definitions.remove(&missing_id);
        let before = registry
            .definition(&preserved_id)
            .expect("first built-in remains registered");

        registry
            .apply_evidence_enforcement(definitions, false)
            .expect_err("a missing later definition must reject the entire policy change");

        let after = registry
            .definition(&preserved_id)
            .expect("failed policy change must preserve the earlier definition");
        assert_eq!(after, before);
    }

    #[test]
    fn non_empty_policy_fixture_rejects_when_enabled_and_accepts_when_disabled() {
        let mut definition = builtin_definitions()
            .into_iter()
            .find(|definition| definition.id == "prompt_task")
            .expect("prompt_task is a built-in definition");
        definition.allowlist = definition.allowlist.require_evidence(
            "implementing",
            "done",
            ["prompt_completion_evidence"],
        );
        let fixture = definition.clone();
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        registry
            .apply_evidence_enforcement([fixture.clone()], true)
            .expect("enabled startup policy installs the evidence contract");

        let instance = WorkflowInstance::new(
            "prompt_task",
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task-1"),
        );
        let decision = WorkflowDecision::new(
            instance.id.clone(),
            "implementing",
            "agent_reported_done",
            "done",
            "agent reported completion",
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::MarkDone,
            "task-1-done",
            json!({ "reason": "done" }),
        ));
        let context = ValidationContext::new("runtime", Utc::now());

        let rejection = registry
            .decision_validator_for_instance(&instance)
            .expect("fixture is not declarative")
            .expect("prompt_task validator exists")
            .validate(&instance, &decision, &context)
            .expect_err("enabled policy must reject missing evidence");
        assert_eq!(
            rejection.kind,
            WorkflowDecisionRejectionKind::MissingRequiredEvidence
        );

        registry
            .apply_evidence_enforcement([fixture], false)
            .expect("disabled startup policy strips the evidence contract");
        registry
            .decision_validator_for_instance(&instance)
            .expect("fixture is not declarative")
            .expect("prompt_task validator exists")
            .validate(&instance, &decision, &context)
            .expect("disabled policy must accept the same unevidenced decision");
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
