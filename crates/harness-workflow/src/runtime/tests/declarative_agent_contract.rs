mod declarative_agent_contract_tests {
    use super::super::*;
    use crate::runtime::model::WorkflowSubject;
    use harness_core::config::workflow::{
        AgentContractMutationPolicy, AgentContractToolPolicy, AgentContractWorkspacePolicy,
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowAgentContract,
        WorkflowDefinitionPolicy,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn contract() -> WorkflowAgentContract {
        WorkflowAgentContract {
            input_schema: "harness.semantic_activity_input.v1".to_string(),
            output_schema: "harness.semantic_verdict.v1".to_string(),
            allowed_outcomes: vec!["small".to_string(), "large".to_string()],
            tools: AgentContractToolPolicy::None,
            mutation: AgentContractMutationPolicy::Forbidden,
            workspace: AgentContractWorkspacePolicy::EphemeralEmpty,
            fresh_context: true,
            max_primary_attempts: 1,
            max_corrections: 1,
        }
    }

    fn classifier_policy() -> WorkflowDefinitionPolicy {
        WorkflowDefinitionPolicy {
            id: "scope_classification".to_string(),
            initial: "classifying".to_string(),
            states: BTreeMap::from([
                (
                    "classifying".to_string(),
                    DeclaredState {
                        activity: Some("classify_scope".to_string()),
                        on_failure: Some("blocked".to_string()),
                        on_signal: BTreeMap::from([
                            ("small".to_string(), "implementing".to_string()),
                            ("large".to_string(), "blocked".to_string()),
                        ]),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "implementing".to_string(),
                    DeclaredState {
                        activity: Some("implement_change".to_string()),
                        on_success: Some("done".to_string()),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["classifying".to_string()],
            intake: None,
        }
    }

    fn activity_policies(
        classifier_contract: Option<WorkflowAgentContract>,
    ) -> BTreeMap<String, WorkflowActivityPolicy> {
        BTreeMap::from([
            (
                "classify_scope".to_string(),
                WorkflowActivityPolicy {
                    agent_contract: classifier_contract,
                    ..WorkflowActivityPolicy::default()
                },
            ),
            (
                "implement_change".to_string(),
                WorkflowActivityPolicy::default(),
            ),
        ])
    }

    #[test]
    fn referenced_contract_is_pinned_and_changes_definition_identity() -> anyhow::Result<()> {
        let policy = classifier_policy();
        let without_contract = build_declarative_definition(&policy, &activity_policies(None))?;
        let with_contract =
            build_declarative_definition(&policy, &activity_policies(Some(contract())))?;

        assert!(without_contract.activity_contracts().is_empty());
        assert_eq!(
            with_contract.agent_contract("classify_scope"),
            Some(&contract())
        );
        assert_ne!(
            without_contract.definition_hash(),
            with_contract.definition_hash(),
            "a referenced agent contract must participate in the definition identity"
        );

        let mut changed_contract = contract();
        changed_contract.max_corrections = 0;
        let with_changed_contract =
            build_declarative_definition(&policy, &activity_policies(Some(changed_contract)))?;
        assert_ne!(
            with_contract.definition_hash(),
            with_changed_contract.definition_hash(),
            "every contract field change must produce a new definition hash"
        );
        Ok(())
    }

    #[test]
    fn unreferenced_contract_does_not_change_identity_or_block_compilation() -> anyhow::Result<()>
    {
        let policy = classifier_policy();
        let baseline = build_declarative_definition(&policy, &activity_policies(None))?;

        let mut invalid_contract = contract();
        invalid_contract.input_schema = "harness.unknown_input.v1".to_string();
        let mut policies = activity_policies(None);
        policies.insert(
            "unreferenced_semantic_activity".to_string(),
            WorkflowActivityPolicy {
                agent_contract: Some(invalid_contract),
                ..WorkflowActivityPolicy::default()
            },
        );
        let with_unreferenced = build_declarative_definition(&policy, &policies)?;
        assert_eq!(
            baseline.definition_hash(),
            with_unreferenced.definition_hash(),
            "an unreferenced global activity policy must not change a definition hash"
        );
        assert!(with_unreferenced.activity_contracts().is_empty());
        Ok(())
    }

    #[test]
    fn invalid_referenced_contract_fails_compilation() {
        let mut invalid = contract();
        invalid.input_schema = "harness.unknown_input.v1".to_string();
        let error =
            build_declarative_definition(&classifier_policy(), &activity_policies(Some(invalid)))
                .expect_err("unsupported input schema must fail compilation");
        assert!(error.to_string().contains("input_schema"), "{error}");
    }

    #[test]
    fn contract_state_routes_must_cover_outcomes_exactly() {
        let mut missing_route = classifier_policy();
        missing_route
            .states
            .get_mut("classifying")
            .expect("fixture state")
            .on_signal
            .remove("large");
        let error =
            build_declarative_definition(&missing_route, &activity_policies(Some(contract())))
                .expect_err("missing outcome route must fail");
        assert!(error.to_string().contains("missing on_signal"), "{error}");

        let mut extra_route = classifier_policy();
        extra_route
            .states
            .get_mut("classifying")
            .expect("fixture state")
            .on_signal
            .insert("medium".to_string(), "blocked".to_string());
        let error = build_declarative_definition(&extra_route, &activity_policies(Some(contract())))
            .expect_err("route outside the outcome vocabulary must fail");
        assert!(error.to_string().contains("outside"), "{error}");

        let mut success_route = classifier_policy();
        success_route
            .states
            .get_mut("classifying")
            .expect("fixture state")
            .on_success = Some("implementing".to_string());
        let error =
            build_declarative_definition(&success_route, &activity_policies(Some(contract())))
                .expect_err("on_success must be rejected for contract states");
        assert!(error.to_string().contains("on_success"), "{error}");
    }

    #[test]
    fn persisted_contract_definition_round_trips_through_hydration() -> anyhow::Result<()> {
        let definition = build_declarative_definition(
            &classifier_policy(),
            &activity_policies(Some(contract())),
        )?;
        let persisted = persisted_declarative_definition(&definition, Some("WORKFLOW.md"));
        assert_eq!(
            persisted.metadata.get("schema_version").and_then(|v| v.as_u64()),
            Some(2),
            "contract-bearing definitions persist the v2 metadata layout"
        );

        let hydrated = hydrate_persisted_declarative_definition(&persisted)?;
        assert_eq!(hydrated.definition_hash(), definition.definition_hash());
        assert_eq!(
            hydrated.activity_contracts(),
            definition.activity_contracts()
        );
        Ok(())
    }

    #[test]
    fn contract_free_definition_keeps_v1_metadata_layout() -> anyhow::Result<()> {
        let definition =
            build_declarative_definition(&classifier_policy(), &activity_policies(None))?;
        let persisted = persisted_declarative_definition(&definition, None);
        assert_eq!(
            persisted.metadata.get("schema_version").and_then(|v| v.as_u64()),
            Some(1),
            "definitions without contracts keep the pre-contract metadata layout"
        );
        assert!(persisted.metadata.get("agent_contracts").is_none());
        hydrate_persisted_declarative_definition(&persisted)?;
        Ok(())
    }

    #[test]
    fn submission_command_pins_contract_into_payload() -> anyhow::Result<()> {
        let definition = build_declarative_definition(
            &classifier_policy(),
            &activity_policies(Some(contract())),
        )?;
        let instance = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            definition.policy().initial.clone(),
            WorkflowSubject::new("test", "submission"),
        )
        .with_server_data(json!({ "definition_hash": definition.definition_hash() }));

        let decision = build_declarative_submission_decision(&definition, &instance)?;
        let command = decision
            .commands
            .first()
            .expect("submission must enqueue the initial activity");
        assert_eq!(
            command.command.get("activity").and_then(|v| v.as_str()),
            Some("classify_scope")
        );
        assert_eq!(
            command.command.get("agent_contract"),
            Some(&serde_json::to_value(contract())?),
            "the runtime job snapshot must carry the pinned contract"
        );
        assert_eq!(
            command.command.get("definition_hash").and_then(|v| v.as_str()),
            Some(definition.definition_hash())
        );
        Ok(())
    }

    #[test]
    fn submission_command_stays_minimal_without_contract() -> anyhow::Result<()> {
        let definition =
            build_declarative_definition(&classifier_policy(), &activity_policies(None))?;
        let instance = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            definition.policy().initial.clone(),
            WorkflowSubject::new("test", "submission"),
        )
        .with_server_data(json!({ "definition_hash": definition.definition_hash() }));

        let decision = build_declarative_submission_decision(&definition, &instance)?;
        let command = decision
            .commands
            .first()
            .expect("submission must enqueue the initial activity");
        assert_eq!(
            command.command,
            json!({ "activity": "classify_scope" }),
            "non-contract activities keep the existing command payload"
        );
        Ok(())
    }
}
