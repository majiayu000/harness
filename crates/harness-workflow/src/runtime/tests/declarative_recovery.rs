mod declarative_recovery {
    use super::super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use harness_core::db::resolve_database_url;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, OnceLock};

    #[test]
    fn declarative_recovery_validator_requires_operator_action_context() -> anyhow::Result<()> {
        let definition = test_definition();
        let instance = stopped_instance(&definition, "decl-recovery-context");
        let validator = decision_validator_for_instance(&instance)
            .ok_or_else(|| anyhow::anyhow!("declarative recovery validator did not resolve"))?;
        let decision = WorkflowDecision::new(
            &instance.id,
            "blocked",
            "operator_runtime_unblock",
            "running",
            "operator selected the declared recovery target",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            "execute_release",
            "decl-recovery-context:running:event",
        ));

        let rejection = validator
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime-agent", chrono::Utc::now()),
            )
            .expect_err("non-operator context must not validate declarative recovery");
        assert_eq!(
            rejection.kind,
            WorkflowDecisionRejectionKind::TransitionNotAllowed
        );
        validator.validate(
            &instance,
            &decision,
            &ValidationContext::new("workflow_runtime_operator_action", chrono::Utc::now()),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn declared_targets_use_the_pinned_state_driver() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let definition = test_definition();
        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("declarative-recovery.db")).await?;

        for (id, target, expected_type) in [
            (
                "decl-recovery-activity",
                "running",
                WorkflowCommandType::EnqueueActivity,
            ),
            ("decl-recovery-wait", "waiting", WorkflowCommandType::Wait),
            (
                "decl-recovery-operator",
                "operator_review",
                WorkflowCommandType::RequestOperatorAttention,
            ),
        ] {
            let instance = stopped_instance(&definition, id);
            store.upsert_instance(&instance).await?;
            let outcome = recover(&store, &instance.id, target).await?;
            let WorkflowRuntimeRecoveryOutcome::Recovered { workflow, .. } = outcome else {
                anyhow::bail!("declared target '{target}' should recover");
            };
            assert_eq!(workflow.state, target);
            let commands = store.commands_for(&instance.id).await?;
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].command.command_type, expected_type);
            assert!(commands[0]
                .command
                .dedupe_key
                .starts_with(&format!("{}:{}:", instance.id, target)));
            match target {
                "running" => {
                    assert_eq!(commands[0].command.activity_name(), Some("execute_release"));
                }
                "waiting" => assert_eq!(
                    commands[0].command.command,
                    json!({ "reason": "declarative workflow is waiting in state 'waiting'" })
                ),
                "operator_review" => {
                    assert_eq!(commands[0].command.command, json!({ "state": target }));
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn recovery_rejects_missing_target_wrong_action_state_and_pin() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }
        let definition = test_definition();
        let dir = tempfile::tempdir()?;
        let store =
            WorkflowRuntimeStore::open(&dir.path().join("declarative-rejections.db")).await?;

        let cases = [
            ("decl-recovery-missing", None, "missing"),
            ("decl-recovery-empty", Some("  "), "missing"),
            ("decl-recovery-terminal", Some("completed"), "invalid"),
            ("decl-recovery-undeclared", Some("unknown"), "invalid"),
        ];
        for (id, target, expected) in cases {
            let instance = stopped_instance(&definition, id);
            store.upsert_instance(&instance).await?;
            let outcome = recover_optional(&store, &instance.id, target).await?;
            assert!(match expected {
                "missing" => matches!(
                    outcome,
                    WorkflowRuntimeRecoveryOutcome::MissingRecoveryTarget { .. }
                ),
                "invalid" => matches!(
                    outcome,
                    WorkflowRuntimeRecoveryOutcome::InvalidRecoveryTarget { .. }
                ),
                _ => false,
            });
            assert_unchanged(&store, &instance).await?;
        }

        let retry = stopped_instance(&definition, "decl-recovery-retry");
        store.upsert_instance(&retry).await?;
        let outcome = store
            .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
                workflow_id: &retry.id,
                action: WorkflowRuntimeRecoveryAction::Retry,
                reason: "operator recovery",
                actor: "operator",
                target_state: Some("running"),
            })
            .await?;
        assert!(matches!(
            outcome,
            WorkflowRuntimeRecoveryOutcome::UnsupportedRecoveryAction { .. }
        ));
        assert_unchanged(&store, &retry).await?;

        let wrong_state = stopped_instance(&definition, "decl-recovery-state")
            .with_data(json!({ "definition_hash": definition.definition_hash() }));
        let mut wrong_state = wrong_state;
        wrong_state.state = "running".to_string();
        store.upsert_instance(&wrong_state).await?;
        assert!(matches!(
            recover(&store, &wrong_state.id, "running").await?,
            WorkflowRuntimeRecoveryOutcome::DeclarativeWrongState { .. }
        ));
        assert_unchanged(&store, &wrong_state).await?;

        for instance in [
            WorkflowInstance::new(
                definition.policy().id.clone(),
                definition.definition_version().wrapping_add(1),
                "blocked",
                WorkflowSubject::new("release", "release:wrong-version"),
            )
            .with_id("decl-recovery-version")
            .with_data(json!({ "definition_hash": definition.definition_hash() })),
            stopped_instance(&definition, "decl-recovery-hash")
                .with_data(json!({ "definition_hash": "wrong-hash" })),
            stopped_instance(&definition, "decl-recovery-no-hash").with_data(json!({})),
        ] {
            store.upsert_instance(&instance).await?;
            assert!(matches!(
                recover(&store, &instance.id, "running").await?,
                WorkflowRuntimeRecoveryOutcome::DefinitionPinMismatch { .. }
            ));
            assert_unchanged(&store, &instance).await?;
        }
        Ok(())
    }

    async fn recover(
        store: &WorkflowRuntimeStore,
        workflow_id: &str,
        target_state: &str,
    ) -> anyhow::Result<WorkflowRuntimeRecoveryOutcome> {
        recover_optional(store, workflow_id, Some(target_state)).await
    }

    async fn recover_optional(
        store: &WorkflowRuntimeStore,
        workflow_id: &str,
        target_state: Option<&str>,
    ) -> anyhow::Result<WorkflowRuntimeRecoveryOutcome> {
        store
            .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
                workflow_id,
                action: WorkflowRuntimeRecoveryAction::Unblock,
                reason: "operator selected a declared recovery target",
                actor: "operator",
                target_state,
            })
            .await
    }

    async fn assert_unchanged(
        store: &WorkflowRuntimeStore,
        expected: &WorkflowInstance,
    ) -> anyhow::Result<()> {
        assert_eq!(
            store.get_instance(&expected.id).await?,
            Some(expected.clone())
        );
        assert!(store.events_for(&expected.id).await?.is_empty());
        assert!(store.commands_for(&expected.id).await?.is_empty());
        Ok(())
    }

    fn stopped_instance(definition: &DeclarativeWorkflowDefinition, id: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "blocked",
            WorkflowSubject::new("release", format!("release:{id}")),
        )
        .with_id(id)
        .with_data(json!({ "definition_hash": definition.definition_hash() }))
    }

    fn test_definition() -> Arc<DeclarativeWorkflowDefinition> {
        static DEFINITION: OnceLock<Arc<DeclarativeWorkflowDefinition>> = OnceLock::new();
        DEFINITION
            .get_or_init(|| {
                let policy = WorkflowDefinitionPolicy {
                    id: "declarative_recovery_test".to_string(),
                    initial: "running".to_string(),
                    states: BTreeMap::from([
                        (
                            "blocked".to_string(),
                            DeclaredState {
                                progress: Some(DeclaredProgressMode::OperatorGate),
                                ..DeclaredState::default()
                            },
                        ),
                        (
                            "operator_review".to_string(),
                            DeclaredState {
                                progress: Some(DeclaredProgressMode::OperatorGate),
                                ..DeclaredState::default()
                            },
                        ),
                        (
                            "running".to_string(),
                            DeclaredState {
                                activity: Some("execute_release".to_string()),
                                on_success: Some("completed".to_string()),
                                on_failure: Some("failed".to_string()),
                                on_signal: BTreeMap::from([(
                                    "cancel".to_string(),
                                    "cancelled".to_string(),
                                )]),
                                ..DeclaredState::default()
                            },
                        ),
                        (
                            "waiting".to_string(),
                            DeclaredState {
                                progress: Some(DeclaredProgressMode::ExternalWait),
                                ..DeclaredState::default()
                            },
                        ),
                    ]),
                    terminal: BTreeMap::from([
                        ("cancelled".to_string(), "cancelled".to_string()),
                        ("completed".to_string(), "succeeded".to_string()),
                        ("failed".to_string(), "failed".to_string()),
                    ]),
                    evidence_required: BTreeMap::new(),
                    recovery_targets: vec![
                        "running".to_string(),
                        "waiting".to_string(),
                        "operator_review".to_string(),
                    ],
                };
                let definition = build_declarative_definition(
                    &policy,
                    &BTreeMap::from([(
                        "execute_release".to_string(),
                        WorkflowActivityPolicy::default(),
                    )]),
                )
                .expect("recovery fixture should compile");
                register_declarative_workflow_definitions([definition])
                    .expect("recovery fixture should register");
                current_declarative_workflow_definition(&policy.id)
                    .expect("registered recovery fixture should resolve")
            })
            .clone()
    }
}
