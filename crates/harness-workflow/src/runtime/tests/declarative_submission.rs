mod declarative_submission {
    use super::super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn definition(
        mut initial: DeclaredState,
    ) -> anyhow::Result<crate::runtime::DeclarativeWorkflowDefinition> {
        initial
            .on_success
            .get_or_insert_with(|| "done".to_string());
        initial
            .on_failure
            .get_or_insert_with(|| "failed".to_string());
        initial
            .on_signal
            .entry("cancel".to_string())
            .or_insert_with(|| "cancelled".to_string());
        let policy = WorkflowDefinitionPolicy {
            id: "declarative_submission_fixture".to_string(),
            initial: "start".to_string(),
            states: BTreeMap::from([
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                ("start".to_string(), initial),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["start".to_string()],
        };
        build_declarative_definition(
            &policy,
            &BTreeMap::from([("review".to_string(), WorkflowActivityPolicy::default())]),
        )
    }

    fn instance_for(
        definition: &crate::runtime::DeclarativeWorkflowDefinition,
    ) -> WorkflowInstance {
        WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            definition.policy().initial.clone(),
            WorkflowSubject::new("declarative", "submission-1"),
        )
        .with_id("declarative-submission-1")
        .with_data(json!({ "definition_hash": definition.definition_hash() }))
    }

    #[test]
    fn activity_initial_state_emits_exact_submission_driver() -> anyhow::Result<()> {
        let definition = definition(DeclaredState {
            activity: Some("review".to_string()),
            on_success: Some("done".to_string()),
            on_failure: Some("failed".to_string()),
            ..DeclaredState::default()
        })?;
        let instance = instance_for(&definition);

        let decision = build_declarative_submission_decision(&definition, &instance)?;

        assert_eq!(decision.observed_state, "start");
        assert_eq!(decision.next_state, "start");
        assert_eq!(decision.commands.len(), 1);
        assert_eq!(
            decision.commands[0].command_type,
            WorkflowCommandType::EnqueueActivity
        );
        assert_eq!(decision.commands[0].activity_name(), Some("review"));
        assert_eq!(
            decision.commands[0].dedupe_key,
            "declarative-submission-1:start:submit"
        );
        register_declarative_workflow_definitions([definition])?;
        decision_validator_for_instance(&instance)
            .map_err(|error| anyhow::anyhow!("unexpected pin error: {error:?}"))?
            .ok_or_else(|| anyhow::anyhow!("declarative validator must be registered"))?
            .validate(
            &instance,
            &decision,
            &ValidationContext::new("submission-test", chrono::Utc::now()),
        )?;
        Ok(())
    }

    #[test]
    fn external_wait_and_operator_gate_initial_states_emit_owned_progress() -> anyhow::Result<()> {
        for (mode, expected) in [
            (
                DeclaredProgressMode::ExternalWait,
                WorkflowCommandType::Wait,
            ),
            (
                DeclaredProgressMode::OperatorGate,
                WorkflowCommandType::RequestOperatorAttention,
            ),
        ] {
            let definition = definition(DeclaredState {
                progress: Some(mode),
                ..DeclaredState::default()
            })?;
            let instance = instance_for(&definition);
            let decision = build_declarative_submission_decision(&definition, &instance)?;
            assert_eq!(decision.commands[0].command_type, expected);
        }
        Ok(())
    }

    #[test]
    fn mismatched_pin_is_rejected_before_command_creation() -> anyhow::Result<()> {
        let definition = definition(DeclaredState {
            activity: Some("review".to_string()),
            on_success: Some("done".to_string()),
            ..DeclaredState::default()
        })?;
        let instance = instance_for(&definition).with_data(json!({
            "definition_hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        }));

        let error = build_declarative_submission_decision(&definition, &instance)
            .expect_err("mismatched pin must fail");
        assert!(error.to_string().contains("is not pinned"));
        Ok(())
    }
}
