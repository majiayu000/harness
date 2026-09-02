const DECLARATIVE_VISIBILITY_DEFINITION_ID: &str = "operator_monitor_visibility_flow";

/// Register a uniquely-named declarative definition into the process-global
/// registry (GH-1609 fixture pattern, `Once`-guarded so it is idempotent across
/// tests in this binary). It carries no built-in instances, so counting tests in
/// sibling suites are unaffected.
fn declarative_visibility_registry() -> WorkflowDefinitionRegistry {
    let mut registry = WorkflowDefinitionRegistry::with_builtins();
    registry
        .register_declarative_historical(declarative_visibility_definition("done", "failed"))
        .expect("historical visibility fixture definition should register");
    registry
        .register_declarative_current(declarative_visibility_definition("completed", "rejected"))
        .expect("visibility fixture definition should register");
    registry
}

fn declarative_visibility_definition(
    success_state: &str,
    failure_state: &str,
) -> harness_workflow::runtime::DeclarativeWorkflowDefinition {
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    let policy = WorkflowDefinitionPolicy {
        id: DECLARATIVE_VISIBILITY_DEFINITION_ID.to_string(),
        initial: "working".to_string(),
        states: BTreeMap::from([
            (
                "working".to_string(),
                DeclaredState {
                    activity: Some("perform_work".to_string()),
                    on_success: Some(success_state.to_string()),
                    on_failure: Some(failure_state.to_string()),
                    on_blocked: Some("blocked".to_string()),
                    on_signal: BTreeMap::from([
                        ("cancel".to_string(), "cancelled".to_string()),
                        ("review".to_string(), "manual_review".to_string()),
                    ]),
                    ..DeclaredState::default()
                },
            ),
            (
                "manual_review".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
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
            (success_state.to_string(), "succeeded".to_string()),
            (failure_state.to_string(), "failed".to_string()),
            ("cancelled".to_string(), "cancelled".to_string()),
        ]),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec!["working".to_string()],
        intake: None,
    };
    harness_workflow::runtime::build_declarative_definition(
        &policy,
        &BTreeMap::from([(
            "perform_work".to_string(),
            WorkflowActivityPolicy::default(),
        )]),
    )
    .expect("visibility fixture definition should be valid")
}

/// Non-DB guard that the visibility fixture builds and registers. The DB-gated
/// visibility test skips before touching the fixture when no database is
/// configured, so without this the fixture's declarative validity is never
/// exercised locally.
#[test]
fn declarative_visibility_fixture_definition_is_valid() {
    let registry = declarative_visibility_registry();
    assert!(
        registry
            .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
            .is_some(),
        "visibility fixture definition should register"
    );
}

#[test]
fn declarative_command_driven_state_counts_as_running() {
    let registry = declarative_visibility_registry();
    let definition = registry
        .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
        .expect("visibility fixture definition should be registered");
    let workflow = WorkflowInstance::new(
        DECLARATIVE_VISIBILITY_DEFINITION_ID,
        definition.definition_version(),
        "working",
        WorkflowSubject::new("issue", "issue:declarative-running"),
    )
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));

    let counts = runtime_workflow_counts(&registry, &[workflow]);

    assert_eq!(counts.running, 1);
    assert_eq!(counts.pending, 0);
}

#[test]
fn workflow_sample_truncation_preserves_declarative_failed_terminal() {
    let registry = declarative_visibility_registry();
    let definition = registry
        .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
        .expect("visibility fixture definition should be registered");
    let failed = WorkflowInstance::new(
        DECLARATIVE_VISIBILITY_DEFINITION_ID,
        definition.definition_version(),
        "rejected",
        WorkflowSubject::new("issue", "issue:declarative-failed"),
    )
    .with_id("declarative-failed")
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
    let mut active = WorkflowInstance::new(
        DECLARATIVE_VISIBILITY_DEFINITION_ID,
        definition.definition_version(),
        "working",
        WorkflowSubject::new("issue", "issue:declarative-active"),
    )
    .with_id("declarative-active")
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));
    active.updated_at = failed.updated_at + chrono::Duration::seconds(1);
    let mut workflows = vec![active, failed];

    truncate_workflow_sample(&registry, &mut workflows, 1);

    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].id, "declarative-failed");
}

#[test]
fn declarative_failed_terminal_populates_failure_and_action_surfaces() {
    let registry = declarative_visibility_registry();
    let definition = registry
        .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
        .expect("visibility fixture definition should be registered");
    let failed = WorkflowInstance::new(
        DECLARATIVE_VISIBILITY_DEFINITION_ID,
        definition.definition_version(),
        "rejected",
        WorkflowSubject::new("issue", "issue:declarative-rejected"),
    )
    .with_id("declarative-rejected")
    .with_server_data(json!({
        "definition_hash": definition.definition_hash(),
        "failure_reason": "declarative review rejected"
    }));

    let failures = grouped_failures(&registry, &[], std::slice::from_ref(&failed));
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].message, "declarative review rejected");

    let actions = operator_actions(
        &registry,
        &[failed],
        Utc::now(),
        &std::collections::HashMap::new(),
    );
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, "failed");
}

#[test]
fn declarative_operator_gate_uses_registry_progress_for_action_kind() {
    let registry = declarative_visibility_registry();
    let definition = registry
        .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
        .expect("visibility fixture definition should be registered");
    let gate = WorkflowInstance::new(
        DECLARATIVE_VISIBILITY_DEFINITION_ID,
        definition.definition_version(),
        "manual_review",
        WorkflowSubject::new("issue", "issue:manual-review"),
    )
    .with_id("declarative-manual-review")
    .with_server_data(json!({ "definition_hash": definition.definition_hash() }));

    let actions = operator_actions(
        &registry,
        &[gate],
        Utc::now(),
        &std::collections::HashMap::new(),
    );
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].kind, "blocked");
}

#[test]
fn declarative_operator_gate_counts_as_blocked_activity() {
    let registry = declarative_visibility_registry();
    let definition = registry
        .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
        .expect("visibility fixture definition should be registered");
    let gate = WorkflowInstance::new(
        DECLARATIVE_VISIBILITY_DEFINITION_ID,
        definition.definition_version(),
        "manual_review",
        WorkflowSubject::new("issue", "issue:manual-review-counts"),
    )
    .with_server_data(json!({
        "definition_hash": definition.definition_hash(),
        "source": "github"
    }));

    let counts = runtime_workflow_counts(&registry, std::slice::from_ref(&gate));
    assert_eq!(counts.blocked, 1);
    assert_eq!(counts.pending, 0);

    let by_source = source_activity(&registry, &[gate], &[]);
    assert_eq!(by_source.len(), 1);
    assert_eq!(by_source[0].source, "github");
    assert_eq!(by_source[0].blocked, 1);
    assert_eq!(by_source[0].pending, 0);
}

#[tokio::test]
async fn declarative_operator_gate_sampling_uses_registry_progress() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let registry = declarative_visibility_registry();
    let definition = registry
        .current_declarative_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID)
        .expect("visibility fixture definition should be registered");
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-gate-progress-")?;
    let store = open_operator_workflow_store(dir.path())
    .await?
    .with_definition_registry(registry.into_shared());
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(
        &store,
        &WorkflowInstance::new(
            DECLARATIVE_VISIBILITY_DEFINITION_ID,
            definition.definition_version(),
            "manual_review",
            WorkflowSubject::new("issue", "issue:manual-review-sample"),
        )
        .with_id("declarative-manual-review-sample")
        .with_server_data(json!({ "definition_hash": definition.definition_hash() })),
    )
    .await?;

    let workflows =
        list_operator_action_workflows(&store, &[DECLARATIVE_VISIBILITY_DEFINITION_ID.to_string()])
            .await?;
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].id, "declarative-manual-review-sample");
    Ok(())
}

/// B-003 / B-005: a blocked instance of a declarative definition surfaces in the
/// operator monitor's runtime-workflow sample (registry-driven enumeration), and
/// a declarative definition with no instances contributes nothing.
#[tokio::test]
async fn declarative_definition_instances_are_visible_in_operator_monitor() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let registry = declarative_visibility_registry();

    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-declarative-")?;
    let store = open_operator_workflow_store(dir.path())
    .await?
    .with_definition_registry(registry.into_shared());

    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(
        &store,
        &WorkflowInstance::new(
            DECLARATIVE_VISIBILITY_DEFINITION_ID,
            1,
            "blocked",
            WorkflowSubject::new("issue", "issue:9001"),
        )
        .with_id("declarative-blocked".to_string())
        .with_server_data(json!({ "repo": "owner/repo", "blocked_reason": "operator gate" })),
    )
    .await?;

    let workflows = list_runtime_workflows_from_store(&store).await?;

    assert!(
        workflows
            .iter()
            .any(|workflow| workflow.id == "declarative-blocked"
                && workflow.definition_id == DECLARATIVE_VISIBILITY_DEFINITION_ID),
        "declarative blocked instance must appear in the operator monitor sample"
    );
    // B-005: no instances were created for any other definition, so the built-ins
    // contribute nothing here — the only sampled workflow is the declarative one.
    assert_eq!(workflows.len(), 1);
    Ok(())
}

#[tokio::test]
async fn declarative_terminal_queries_use_pinned_versions_and_outcomes() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let registry = declarative_visibility_registry();
    let historical = declarative_visibility_definition("done", "failed");
    let current = declarative_visibility_definition("completed", "rejected");

    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-terminal-outcomes-")?;
    let store = open_operator_workflow_store(dir.path())
    .await?
    .with_definition_registry(registry.into_shared());
    for (id, definition, state) in [
        ("historical-success", &historical, "done"),
        ("current-success", &current, "completed"),
        ("historical-failure", &historical, "failed"),
        ("current-failure", &current, "rejected"),
        ("current-active", &current, "working"),
    ] {
        crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(
            &store,
            &WorkflowInstance::new(
                DECLARATIVE_VISIBILITY_DEFINITION_ID,
                definition.definition_version(),
                state,
                WorkflowSubject::new("issue", format!("issue:{id}")),
            )
            .with_id(id.to_string())
            .with_server_data(json!({ "definition_hash": definition.definition_hash() })),
        )
        .await?;
    }

    let active = store
        .list_nonterminal_instances_by_definition(DECLARATIVE_VISIBILITY_DEFINITION_ID, None, None)
        .await?;
    assert_eq!(
        active
            .iter()
            .map(|workflow| workflow.id.as_str())
            .collect::<Vec<_>>(),
        vec!["current-active"]
    );

    let failed = list_recent_failed_workflows(
        &store,
        &[DECLARATIVE_VISIBILITY_DEFINITION_ID.to_string()],
        10,
    )
    .await?;
    let failed_ids = failed
        .iter()
        .map(|workflow| workflow.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        failed_ids,
        std::collections::HashSet::from(["historical-failure", "current-failure"])
    );
    Ok(())
}
