// Hard workflow budget ceiling at activity completion (GH-1770 spec §4.4).
// The reducer's decision stands unless the completed activity pushed the
// instance past its USD budget under `enforce`.

fn ceiling_store_policy(
    budget_usd: f64,
    enforcement: RuntimeBudgetEnforcement,
) -> RuntimeBudgetPolicy {
    RuntimeBudgetPolicy {
        default_workflow_budget_usd: budget_usd,
        enforcement,
        unlimited: false,
        daily_profile_cap_usd: None,
        daily_throttle_ratio: 0.8,
    }
}

fn ceiling_usage_upsert(workflow_id: &str, cost_usd_micros: u64) -> RuntimeUsageUpsert {
    RuntimeUsageUpsert {
        runtime_job_id: format!("ceiling-job-{workflow_id}"),
        command_id: format!("ceiling-command-{workflow_id}"),
        workflow_id: workflow_id.to_string(),
        turn_id: Some(format!("ceiling-turn-{workflow_id}")),
        agent_run_id: None,
        runtime_kind: RuntimeKind::CodexExec,
        runtime_profile: "codex-default".to_string(),
        agent: "codex".to_string(),
        model: "gpt-5".to_string(),
        project: "/project-a".to_string(),
        task_id: None,
        candidate_group_id: None,
        candidate_id: None,
        candidate_index: None,
        candidate_count: None,
        metrics: RuntimeUsageMetrics::default(),
        cost_usd_micros,
        reported_at: Utc::now(),
    }
}

/// A completion that the reducer turns into the progressing `bind_pr` decision
/// (`implementing` -> `pr_open`) — the case the ceiling must be able to stop.
fn progressing_completion_payload() -> anyhow::Result<serde_json::Value> {
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ))
        .with_artifact(verified_pr_binding(77));
    Ok(json!({
        "command_id": "ceiling-command",
        "runtime_job_id": "ceiling-job",
        "activity_result": result,
    }))
}

async fn ceiling_instance(
    store: &WorkflowRuntimeStore,
    id: &str,
    spent_usd: f64,
) -> anyhow::Result<WorkflowInstance> {
    let instance = issue_instance("implementing").with_id(id);
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    store
        .upsert_runtime_usage(&ceiling_usage_upsert(
            &instance.id,
            cost_usd_to_micros(spent_usd)?,
        ))
        .await?;
    Ok(instance)
}

#[tokio::test]
async fn budget_ceiling_enforce_blocks_progressing_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_budget_policy(ceiling_store_policy(15.0, RuntimeBudgetEnforcement::Enforce));
    let instance = ceiling_instance(&store, "budget-ceiling-enforce", 20.0).await?;

    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            progressing_completion_payload()?,
        )
        .await?
        .expect("an over-budget completion still produces a decision");

    assert!(record.accepted, "{:?}", record.rejection_reason);
    assert_eq!(record.decision.decision, "block_budget_exhausted");
    assert_eq!(record.decision.next_state, "blocked");
    assert!(record
        .decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    let operator = record
        .decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention)
        .expect("the ceiling requests operator attention");
    assert_eq!(operator.command["budget"]["spent_usd"], 20.0);
    assert_eq!(operator.command["budget"]["budget_usd"], 15.0);
    assert_eq!(operator.command["budget"]["enforcement"], "enforce");

    let blocked = record
        .decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::MarkBlocked)
        .expect("the ceiling marks the workflow blocked");
    assert_eq!(blocked.command["stop_reason_code"], "budget_exhausted");
    assert_eq!(blocked.command["reason_class"], "terminal");

    let after = store
        .get_instance(&instance.id)
        .await?
        .expect("workflow instance should remain visible");
    assert_eq!(after.state, "blocked");
    Ok(())
}

#[tokio::test]
async fn budget_ceiling_shadow_records_event_and_keeps_decision() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_budget_policy(ceiling_store_policy(15.0, RuntimeBudgetEnforcement::Shadow));
    let instance = ceiling_instance(&store, "budget-ceiling-shadow", 20.0).await?;

    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            progressing_completion_payload()?,
        )
        .await?
        .expect("shadow mode still commits the reducer decision");

    assert!(record.accepted, "{:?}", record.rejection_reason);
    assert_eq!(record.decision.decision, "bind_pr");
    assert_eq!(record.decision.next_state, "pr_open");

    let events = store.events_for(&instance.id).await?;
    let shadow = events
        .iter()
        .find(|event| event.event_type == "BudgetShadowDecision")
        .expect("shadow mode records a BudgetShadowDecision event");
    assert_eq!(shadow.event["decision"], "would_block");
    assert_eq!(shadow.event["spent_usd"], 20.0);
    assert_eq!(shadow.event["budget_usd"], 15.0);
    assert_eq!(shadow.event["reducer_decision"], "bind_pr");
    assert_eq!(shadow.event["reducer_next_state"], "pr_open");
    Ok(())
}

#[tokio::test]
async fn budget_ceiling_leaves_under_budget_completion_untouched() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_budget_policy(ceiling_store_policy(15.0, RuntimeBudgetEnforcement::Enforce));
    let instance = ceiling_instance(&store, "budget-ceiling-under", 5.0).await?;

    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            progressing_completion_payload()?,
        )
        .await?
        .expect("an under-budget completion produces the reducer decision");

    assert_eq!(record.decision.decision, "bind_pr");
    assert_eq!(record.decision.next_state, "pr_open");
    assert!(
        store
            .events_for(&instance.id)
            .await?
            .iter()
            .all(|event| event.event_type != "BudgetShadowDecision"),
        "an under-budget completion records no budget decision"
    );
    Ok(())
}

#[tokio::test]
async fn budget_ceiling_does_not_preempt_terminal_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_budget_policy(ceiling_store_policy(15.0, RuntimeBudgetEnforcement::Enforce));
    let instance = ceiling_instance(&store, "budget-ceiling-terminal", 20.0).await?;

    // A workflow that is finishing anyway must not be rewritten into `blocked`:
    // blocking it would hide the real outcome and strand a done workflow.
    let result = ActivityResult::succeeded(
        "implement_issue",
        "The issue was already closed before implementation completed.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));

    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            json!({
                "command_id": "ceiling-terminal-command",
                "runtime_job_id": "ceiling-terminal-job",
                "activity_result": result,
            }),
        )
        .await?
        .expect("closed issue evidence should produce a domain decision");

    assert_eq!(record.decision.decision, "finish_closed_issue");
    assert_eq!(record.decision.next_state, "done");
    Ok(())
}

#[tokio::test]
async fn budget_ceiling_preempts_terminal_agent_contract_completion() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_budget_policy(ceiling_store_policy(15.0, RuntimeBudgetEnforcement::Enforce));
    let instance = ceiling_instance(&store, "budget-ceiling-terminal-contract", 20.0).await?;
    let result = ActivityResult::succeeded(
        "implement_issue",
        "The issue was already closed before implementation completed.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));

    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            json!({
                "command_id": "ceiling-terminal-contract-command",
                "runtime_job_id": "ceiling-terminal-contract-job",
                "command": {
                    "command_type": "enqueue_activity",
                    "dedupe_key": "ceiling-terminal-contract",
                    "command": {"agent_contract": {"output_schema": "test"}}
                },
                "activity_result": result,
            }),
        )
        .await?
        .expect("over-budget contract completion still produces a decision");

    assert!(record.accepted, "{:?}", record.rejection_reason);
    assert_eq!(record.decision.decision, "block_budget_exhausted");
    assert_eq!(record.decision.next_state, "blocked");
    Ok(())
}

#[tokio::test]
async fn budget_stop_preempts_declarative_on_blocked_terminal_route() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    let definition = crate::runtime::build_declarative_definition(
        &WorkflowDefinitionPolicy {
            id: "budget_stop_terminal_route".to_string(),
            initial: "running".to_string(),
            states: BTreeMap::from([
                (
                    "running".to_string(),
                    DeclaredState {
                        activity: Some("run".to_string()),
                        on_blocked: Some("done".to_string()),
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
            recovery_targets: Vec::new(),
            intake: None,
        },
        &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
    )?;
    let mut registry = crate::runtime::WorkflowDefinitionRegistry::with_builtins();
    registry.register_declarative_current(definition.clone())?;
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_definition_registry(registry.into_shared())
        .with_budget_policy(ceiling_store_policy(0.10, RuntimeBudgetEnforcement::Enforce));
    let instance = WorkflowInstance::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        "running",
        WorkflowSubject::new("test", "budget-stop-terminal-route"),
    )
    .with_id("budget-stop-terminal-route")
    .with_server_data(json!({"definition_hash": definition.definition_hash()}));
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    store
        .upsert_runtime_usage(&ceiling_usage_upsert(&instance.id, 125_000))
        .await?;

    let result = ActivityResult {
        activity: "run".to_string(),
        status: ActivityStatus::Blocked,
        summary: "The runtime stopped the turn at the enforced workflow budget.".to_string(),
        artifacts: vec![ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_RUNTIME_BUDGET_STOP,
            json!({"spent_usd": 0.125, "budget_usd": 0.10}),
        )],
        signals: Vec::new(),
        validation: Vec::new(),
        error: Some("workflow budget exhausted".to_string()),
        error_kind: None,
    };
    let command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "budget-stop-terminal-route-command",
        json!({"activity": "run"}),
    );
    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            json!({
                "command_id": "budget-stop-terminal-route-command",
                "runtime_job_id": "budget-stop-terminal-route-job",
                "command": command,
                "activity_result": result,
            }),
        )
        .await?
        .expect("the budget-stopped completion must produce a decision");

    assert!(record.accepted, "{:?}", record.rejection_reason);
    assert_eq!(record.decision.decision, "block_budget_exhausted");
    assert_eq!(record.decision.next_state, "blocked");
    Ok(())
}

#[tokio::test]
async fn budget_ceiling_unlimited_policy_never_blocks() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db"))
        .await?
        .with_budget_policy(RuntimeBudgetPolicy {
            unlimited: true,
            enforcement: RuntimeBudgetEnforcement::Enforce,
            ..RuntimeBudgetPolicy::default()
        });
    let instance = ceiling_instance(&store, "budget-ceiling-unlimited", 500.0).await?;

    let record = store
        .commit_parent_runtime_completion(
            &instance.id,
            "runtime-1",
            progressing_completion_payload()?,
        )
        .await?
        .expect("unlimited policy commits the reducer decision");

    assert_eq!(record.decision.decision, "bind_pr");
    assert_eq!(record.decision.next_state, "pr_open");
    Ok(())
}
