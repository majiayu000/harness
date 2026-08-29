mod declarative_agent_contract_tests {
    use super::super::*;
    use crate::runtime::model::WorkflowSubject;
    use harness_core::db::resolve_database_url;
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

    const CLASSIFY_PROMPT: &str = "Classify only the supplied facts.";

    fn activity_policies(
        classifier_contract: Option<WorkflowAgentContract>,
    ) -> BTreeMap<String, WorkflowActivityPolicy> {
        BTreeMap::from([
            (
                "classify_scope".to_string(),
                WorkflowActivityPolicy {
                    prompt: Some(CLASSIFY_PROMPT.to_string()),
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

    fn pinned() -> crate::runtime::PinnedAgentContractActivity {
        crate::runtime::PinnedAgentContractActivity {
            prompt: CLASSIFY_PROMPT.to_string(),
            contract: contract(),
        }
    }

    #[test]
    fn referenced_contract_is_pinned_and_changes_definition_identity() -> anyhow::Result<()> {
        let policy = classifier_policy();
        let without_contract = build_declarative_definition(&policy, &activity_policies(None))?;
        let with_contract =
            build_declarative_definition(&policy, &activity_policies(Some(contract())))?;

        assert!(without_contract.activity_contracts().is_empty());
        assert_eq!(with_contract.agent_contract("classify_scope"), Some(&pinned()));
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

        let mut changed_prompt = activity_policies(Some(contract()));
        changed_prompt
            .get_mut("classify_scope")
            .expect("fixture activity")
            .prompt = Some("Classify with a different pinned prompt.".to_string());
        let with_changed_prompt = build_declarative_definition(&policy, &changed_prompt)?;
        assert_ne!(
            with_contract.definition_hash(),
            with_changed_prompt.definition_hash(),
            "the effective prompt participates in the definition identity"
        );
        Ok(())
    }

    #[test]
    fn contract_activity_requires_prompt_and_forbids_validation_commands() {
        let mut missing_prompt = activity_policies(Some(contract()));
        missing_prompt
            .get_mut("classify_scope")
            .expect("fixture activity")
            .prompt = None;
        let error = build_declarative_definition(&classifier_policy(), &missing_prompt)
            .expect_err("a contract activity without a prompt must fail compilation");
        assert!(error.to_string().contains("no prompt"), "{error}");

        let mut with_validation = activity_policies(Some(contract()));
        with_validation
            .get_mut("classify_scope")
            .expect("fixture activity")
            .validation = vec!["cargo check".to_string()];
        let error = build_declarative_definition(&classifier_policy(), &with_validation)
            .expect_err("tools: none cannot run validation commands");
        assert!(error.to_string().contains("validation command"), "{error}");
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
            command.command.get("prompt").and_then(|v| v.as_str()),
            Some(CLASSIFY_PROMPT),
            "the runtime job snapshot must carry the pinned effective prompt"
        );
        assert_eq!(
            command.command.get("definition_hash").and_then(|v| v.as_str()),
            Some(definition.definition_hash())
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatcher_defers_agent_contract_command_until_enforcement_exists(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let definition = build_declarative_definition(
            &classifier_policy(),
            &activity_policies(Some(contract())),
        )?;
        let instance = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "classifying",
            WorkflowSubject::new("test", "agent-contract-defer"),
        )
        .with_id("agent-contract-defer")
        .with_server_data(json!({
            "definition_hash": definition.definition_hash(),
            "project_id": "/project-a",
        }));
        store.force_upsert_lifecycle_state_for_test(&instance).await?;

        let command =
            crate::runtime::declarative_agent_contract::declarative_enqueue_activity_command(
                &definition,
                "classify_scope",
                "agent-contract-defer-1".to_string(),
            );
        let decision = WorkflowDecision::new(
            instance.id.clone(),
            "classifying",
            "enqueue_classifier",
            "classifying",
            "Dispatch the pinned classifier activity.",
        )
        .with_command(command);
        let record = WorkflowDecisionRecord::accepted(decision.clone(), None);
        store.record_decision(&record).await?;
        let command_id = store
            .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
            .await?;

        let dispatcher = RuntimeCommandDispatcher::new(
            &store,
            RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
        );
        let outcome = dispatcher
            .dispatch_once()
            .await?
            .expect("pending command should be considered");
        match outcome {
            CommandDispatchOutcome::Deferred {
                command_id: deferred_command_id,
                barrier,
            } => {
                assert_eq!(deferred_command_id, command_id);
                assert_eq!(
                    barrier.reason_code.as_str(),
                    "agent_contract_enforcement_unavailable"
                );
            }
            other => panic!(
                "agent contract command must defer until enforcement exists, got {other:?}"
            ),
        }
        assert!(
            store
                .runtime_jobs_for_command(&command_id)
                .await?
                .is_empty(),
            "no runtime job may be created for an unenforceable contract"
        );
        Ok(())
    }

    #[tokio::test]
    async fn recovery_rebuilds_agent_contract_command_from_pinned_definition(
    ) -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let definition = build_declarative_definition(
            &classifier_policy(),
            &activity_policies(Some(contract())),
        )?;
        let mut registry = crate::runtime::WorkflowDefinitionRegistry::with_builtins();
        registry.register_declarative_current(definition.clone())?;
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
            .await?
            .with_definition_registry(registry.into_shared());
        let instance = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "blocked",
            WorkflowSubject::new("test", "agent-contract-recovery"),
        )
        .with_id("agent-contract-recovery")
        .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
        store.force_upsert_lifecycle_state_for_test(&instance).await?;

        let recovered = store
            .recover_stopped_instance(crate::runtime::WorkflowRuntimeRecoveryRequest {
                workflow_id: &instance.id,
                action: crate::runtime::WorkflowRuntimeRecoveryAction::Unblock,
                reason: "operator repaired the dependency",
                actor: "operator",
                target_state: Some("classifying"),
                evidence: &[],
            })
            .await?;
        assert!(
            matches!(
                recovered,
                crate::runtime::WorkflowRuntimeRecoveryOutcome::Recovered { .. }
            ),
            "recovery should succeed, got {recovered:?}"
        );

        let commands = store.commands_for(&instance.id).await?;
        assert_eq!(commands.len(), 1);
        let payload = &commands[0].command.command;
        assert_eq!(
            payload.get("activity").and_then(|v| v.as_str()),
            Some("classify_scope")
        );
        assert_eq!(
            payload.get("agent_contract"),
            Some(&serde_json::to_value(contract())?),
            "operator recovery must not drop the pinned agent contract"
        );
        assert_eq!(
            payload.get("prompt").and_then(|v| v.as_str()),
            Some(CLASSIFY_PROMPT)
        );
        assert_eq!(
            payload.get("definition_hash").and_then(|v| v.as_str()),
            Some(definition.definition_hash())
        );
        assert_eq!(
            payload.get("recovery_target").and_then(|v| v.as_str()),
            Some("classifying")
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
