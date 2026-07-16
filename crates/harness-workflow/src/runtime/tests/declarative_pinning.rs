mod declarative_pinning {
    use super::super::*;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use harness_core::db::resolve_database_url;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn activity_policies() -> BTreeMap<String, WorkflowActivityPolicy> {
        BTreeMap::from([("review".to_string(), WorkflowActivityPolicy::default())])
    }

    fn policy_v1() -> WorkflowDefinitionPolicy {
        WorkflowDefinitionPolicy {
            id: "docs_review".to_string(),
            initial: "reviewing".to_string(),
            states: BTreeMap::from([
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "reviewing".to_string(),
                    DeclaredState {
                        activity: Some("review".to_string()),
                        on_success: Some("done".to_string()),
                        on_failure: Some("failed".to_string()),
                        on_signal: BTreeMap::from([(
                            "cancel".to_string(),
                            "cancelled".to_string(),
                        )]),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("cancelled".to_string(), "cancelled".to_string()),
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
            ]),
            evidence_required: BTreeMap::from([(
                "done".to_string(),
                vec!["review_report".to_string()],
            )]),
            recovery_targets: vec!["reviewing".to_string()],
        }
    }

    fn policy_v2() -> WorkflowDefinitionPolicy {
        let mut policy = policy_v1();
        policy
            .states
            .get_mut("reviewing")
            .expect("fixture reviewing state should exist")
            .on_success = Some("completed".to_string());
        let reviewing = policy
            .states
            .get_mut("reviewing")
            .expect("fixture reviewing state should exist");
        reviewing.activity = None;
        reviewing.progress = Some(DeclaredProgressMode::ExternalWait);
        policy.terminal.remove("done");
        policy
            .terminal
            .insert("completed".to_string(), "succeeded".to_string());
        policy.evidence_required.remove("done");
        policy.evidence_required.insert(
            "completed".to_string(),
            vec!["review_report".to_string()],
        );
        policy
    }

    fn compiled(policy: &WorkflowDefinitionPolicy) -> DeclarativeWorkflowDefinition {
        build_declarative_definition(policy, &activity_policies())
            .expect("fixture declaration should compile")
    }

    #[test]
    fn canonical_identity_uses_lower_sha256_bits_and_roundtrips_strictly() {
        let definition = compiled(&policy_v1());
        let suffix = definition
            .definition_hash()
            .get(definition.definition_hash().len() - 8..)
            .expect("sha256 hash should have an eight-character suffix");
        assert_eq!(
            definition.definition_version(),
            u32::from_str_radix(suffix, 16).expect("hash suffix should be hexadecimal")
        );

        let persisted = persisted_declarative_definition(&definition, Some("/repo/WORKFLOW.md"));
        assert_eq!(persisted.source_path.as_deref(), Some("/repo/WORKFLOW.md"));
        assert_eq!(persisted.definition_hash, definition.definition_hash());
        assert_eq!(
            hydrate_declarative_definition(&persisted, &activity_policies())
                .expect("canonical persisted declaration should hydrate"),
            definition
        );

        let mut wrong_hash = persisted.clone();
        wrong_hash.definition_hash = "sha256:wrong".to_string();
        assert!(hydrate_declarative_definition(&wrong_hash, &activity_policies())
            .expect_err("hash mismatch must fail closed")
            .to_string()
            .contains("does not match canonical policy hash"));

        let mut wrong_version = persisted;
        wrong_version.version ^= 1;
        assert!(hydrate_declarative_definition(&wrong_version, &activity_policies())
            .expect_err("version mismatch must fail closed")
            .to_string()
            .contains("does not match canonical policy version"));
    }

    #[test]
    fn registry_preserves_historical_shapes_and_fails_closed_for_missing_versions() {
        let v1 = compiled(&policy_v1());
        let v2 = compiled(&policy_v2());
        assert_ne!(v1.definition_version(), v2.definition_version());

        let mut registry = WorkflowDefinitionRegistry::new_for_tests();
        registry
            .register_declarative_historical(v1.clone())
            .expect("v1 history should register");
        registry
            .register_declarative_current(v2.clone())
            .expect("v2 current should register");

        assert_eq!(
            registry
                .state_definition_for_version("docs_review", v1.definition_version(), "done")
                .and_then(|state| state.terminal_state),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            registry
                .state_definition_for_version(
                    "docs_review",
                    v1.definition_version(),
                    "reviewing",
                )
                .and_then(|state| state.progress_mode),
            Some(WorkflowProgressMode::CommandDriven)
        );
        assert_eq!(
            registry
                .state_definition_for_version(
                    "docs_review",
                    v2.definition_version(),
                    "reviewing",
                )
                .and_then(|state| state.progress_mode),
            Some(WorkflowProgressMode::ExternalWait)
        );
        let v1_instance = WorkflowInstance::new(
            "docs_review",
            v1.definition_version(),
            "done",
            WorkflowSubject::new("document", "one"),
        )
        .with_data(json!({ "definition_hash": v1.definition_hash() }));
        let v2_instance = WorkflowInstance::new(
            "docs_review",
            v2.definition_version(),
            "done",
            WorkflowSubject::new("document", "two"),
        )
        .with_data(json!({ "definition_hash": v2.definition_hash() }));
        assert_eq!(
            registry
                .state_definition_for_instance(&v1_instance, "done")
                .and_then(|state| state.terminal_state),
            Some(WorkflowTerminalState::Succeeded),
        );
        assert!(registry
            .state_definition_for_instance(&v2_instance, "done")
            .is_none());
        let mismatched_hash = WorkflowInstance::new(
            "docs_review",
            v1.definition_version(),
            "done",
            WorkflowSubject::new("document", "mismatch"),
        )
        .with_data(json!({ "definition_hash": v2.definition_hash() }));
        assert!(registry
            .state_definition_for_instance(&mismatched_hash, "done")
            .is_none());
        assert!(registry
            .state_definition_for_version("docs_review", v2.definition_version(), "done")
            .is_none());
        assert_eq!(
            registry
                .state_definition_for_version(
                    "docs_review",
                    v2.definition_version(),
                    "completed",
                )
                .and_then(|state| state.terminal_state),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert!(registry
            .definition_for_version("docs_review", u32::MAX)
            .is_none());
        let missing_unmarked_version = WorkflowInstance::new(
            "docs_review",
            u32::MAX,
            "completed",
            WorkflowSubject::new("document", "missing-unmarked"),
        );
        assert!(registry
            .state_definition_for_instance(&missing_unmarked_version, "completed")
            .is_none());
        assert!(workflow_definition_for_version(GITHUB_ISSUE_PR_DEFINITION_ID, u32::MAX).is_some());
        let builtin_with_unrelated_hash = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            u32::MAX,
            "done",
            WorkflowSubject::new("issue", "1609"),
        )
        .with_data(json!({ "definition_hash": "unrelated-business-metadata" }));
        assert_eq!(
            workflow_state_definition_for_instance(&builtin_with_unrelated_hash, "done")
                .and_then(|state| state.terminal_state),
            Some(WorkflowTerminalState::Succeeded),
        );

        let mut raw_registry = WorkflowDefinitionRegistry::new_for_tests();
        raw_registry
            .register(RegisteredWorkflowDefinition::new(
                "docs_review",
                v1.registered().states.clone(),
                v1.registered().allowlist.clone(),
            ))
            .expect("raw compatibility definition should register");
        let raw_instance = WorkflowInstance::new(
            "docs_review",
            u32::MAX,
            "done",
            WorkflowSubject::new("document", "raw"),
        );
        assert!(raw_registry
            .state_definition_for_instance(&raw_instance, "done")
            .is_some());
        let raw_with_unrelated_hash = raw_instance
            .clone()
            .with_data(json!({ "definition_hash": v1.definition_hash() }));
        assert!(matches!(
            raw_registry.resolve_declarative_definition(&raw_with_unrelated_hash),
            DeclarativeDefinitionResolution::NotDeclarative
        ));
        assert!(raw_registry
            .state_definition_for_instance(&raw_with_unrelated_hash, "done")
            .is_some());
    }

    #[test]
    fn registry_batch_failure_and_namespace_collision_leave_state_unchanged() {
        let v1 = compiled(&policy_v1());
        let v2 = compiled(&policy_v2());
        let mut registry = WorkflowDefinitionRegistry::new_for_tests();
        let error = registry
            .register_declarative_current_batch([v1.clone(), v2.clone()])
            .expect_err("duplicate current ids in one batch must fail");
        assert!(error.to_string().contains("already registered as current"));
        assert!(registry.definition("docs_review").is_none());
        assert!(registry
            .declarative_definition("docs_review", v1.definition_version())
            .is_none());

        registry
            .register_declarative_historical(v1.clone())
            .expect("first historical version should register");
        let raw_collision = RegisteredWorkflowDefinition::new(
            "docs_review",
            v1.registered().states.clone(),
            v1.registered().allowlist.clone(),
        );
        assert!(registry
            .register(raw_collision)
            .expect_err("raw current registration must not shadow declarative history")
            .to_string()
            .contains("registered declarative history"));
        assert_eq!(
            registry
                .declarative_definition("docs_review", v1.definition_version())
                .expect("original historical version should remain")
                .definition_hash(),
            v1.definition_hash()
        );
    }

    #[tokio::test]
    async fn durable_versions_roundtrip_and_reject_hash_collisions() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("declarative-pinning.db")).await?;
        let v1 = compiled(&policy_v1());
        let v2 = compiled(&policy_v2());
        let mut persisted_v1 = persisted_declarative_definition(&v1, Some("/repo/v1/WORKFLOW.md"));
        persisted_v1.active = false;
        let persisted_v2 = persisted_declarative_definition(&v2, Some("/repo/WORKFLOW.md"));
        store.persist_definition_version(&persisted_v1).await?;
        store.persist_definition_version(&persisted_v2).await?;

        let persisted = store.list_definitions().await?;
        assert_eq!(persisted.len(), 2);
        let loaded_v1 = persisted
            .iter()
            .find(|definition| definition.version == v1.definition_version())
            .expect("persisted v1 should be listed");
        let loaded_v2 = persisted
            .iter()
            .find(|definition| definition.version == v2.definition_version())
            .expect("persisted v2 should be listed");
        let hydrated_v1 = hydrate_declarative_definition(loaded_v1, &activity_policies())?;
        let hydrated_v2 = hydrate_declarative_definition(loaded_v2, &activity_policies())?;
        let mut registry = WorkflowDefinitionRegistry::new_for_tests();
        registry.register_declarative_historical(hydrated_v1)?;
        registry.register_declarative_current(hydrated_v2)?;
        assert!(registry
            .declarative_definition("docs_review", v1.definition_version())
            .is_some());
        assert_eq!(
            registry
                .current_declarative_definition("docs_review")
                .expect("v2 should remain current")
                .definition_version(),
            v2.definition_version()
        );

        let mut same_hash_different_metadata = persisted_v1.clone();
        same_hash_different_metadata
            .metadata
            .as_object_mut()
            .expect("declarative metadata should be an object")
            .insert("tampered".to_string(), json!(true));
        let error = store
            .persist_definition_version(&same_hash_different_metadata)
            .await
            .expect_err("same hash with different metadata must remain immutable");
        assert!(error.to_string().contains("immutable payload conflicts"));

        let mut mutable_upsert = persisted_v1.clone();
        mutable_upsert.name = "Mutable overwrite".to_string();
        mutable_upsert.metadata = json!({ "kind": "generic" });
        let error = store
            .upsert_definition(&mutable_upsert)
            .await
            .expect_err("generic upsert must not overwrite a declarative row");
        assert!(error.to_string().contains("cannot be overwritten"));

        let mut collision = persisted_v1.clone();
        collision.definition_hash = "sha256:different".to_string();
        let error = store
            .persist_definition_version(&collision)
            .await
            .expect_err("durable version collision must fail");
        assert!(error.to_string().contains("version collision"));
        assert_eq!(
            store
                .get_definition("docs_review", v1.definition_version())
                .await?
                .expect("original durable version should remain")
                .definition_hash,
            persisted_v1.definition_hash
        );
        Ok(())
    }
}
