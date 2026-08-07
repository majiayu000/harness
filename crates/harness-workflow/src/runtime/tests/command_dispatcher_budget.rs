use crate::runtime::{cost_usd_to_micros, RuntimeUsageMetrics, RuntimeUsageUpsert};
use harness_core::config::workflow::{RuntimeBudgetEnforcement, RuntimeBudgetPolicy};

fn budget_usage_upsert(workflow_id: &str, cost_usd_micros: u64) -> RuntimeUsageUpsert {
    RuntimeUsageUpsert {
        runtime_job_id: format!("runtime-job-{workflow_id}"),
        command_id: format!("command-{workflow_id}"),
        workflow_id: workflow_id.to_string(),
        turn_id: Some(format!("turn-{workflow_id}")),
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

fn enforce_budget_policy(budget_usd: f64) -> RuntimeBudgetPolicy {
    RuntimeBudgetPolicy {
        default_workflow_budget_usd: budget_usd,
        enforcement: RuntimeBudgetEnforcement::Enforce,
        unlimited: false,
    }
}

async fn issue_with_pending_command(
    store: &WorkflowRuntimeStore,
    issue_number: u64,
) -> anyhow::Result<(WorkflowInstance, String)> {
    let instance = project_issue_instance("/project-a", issue_number, "replanning");
    store.force_upsert_lifecycle_state_for_test(&instance).await?;
    let command = WorkflowCommand::enqueue_activity(
        "replan_issue",
        format!("issue-{issue_number}-replan-budget"),
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    Ok((instance, command_id))
}

#[tokio::test]
async fn budget_gate_shadow_records_event_and_dispatches() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_command(&store, 501).await?;
    store
        .upsert_runtime_usage(&budget_usage_upsert(&instance.id, cost_usd_to_micros(20.0)?))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    );
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "shadow mode must dispatch despite the exhausted budget: {outcome:?}"
    );
    let events = store.events_for(&instance.id).await?;
    let shadow = events
        .iter()
        .find(|event| event.event_type == "BudgetShadowDecision")
        .expect("shadow mode records a BudgetShadowDecision event");
    assert_eq!(shadow.source, "workflow_runtime_command_dispatcher");
    assert_eq!(shadow.event["decision"], "would_defer");
    assert_eq!(
        shadow.event["barrier_reason_code"],
        "workflow_budget_exhausted"
    );
    assert_eq!(shadow.event["budget_usd"], 15.0);
    assert_eq!(shadow.event["spent_usd"], 20.0);
    Ok(())
}

#[tokio::test]
async fn budget_gate_enforce_defers_with_budget_barrier() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, command_id) = issue_with_pending_command(&store, 502).await?;
    store
        .upsert_runtime_usage(&budget_usage_upsert(&instance.id, cost_usd_to_micros(16.0)?))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    )
    .with_budget_policy(enforce_budget_policy(15.0));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    let barrier = match outcome {
        CommandDispatchOutcome::Deferred { command_id: id, barrier } => {
            assert_eq!(id, command_id);
            barrier
        }
        other => panic!("enforce mode must defer an over-budget command: {other:?}"),
    };
    assert_eq!(
        barrier.reason_code,
        DispatchBarrierReasonCode::WorkflowBudgetExhausted
    );
    assert!(barrier.reason.contains("16.00"), "{}", barrier.reason);
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Deferred
    );
    assert!(store
        .events_for(&instance.id)
        .await?
        .iter()
        .all(|event| event.event_type != "BudgetShadowDecision"));
    Ok(())
}

#[tokio::test]
async fn budget_gate_enforce_dispatches_under_budget() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_command(&store, 503).await?;
    store
        .upsert_runtime_usage(&budget_usage_upsert(&instance.id, cost_usd_to_micros(1.0)?))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    )
    .with_budget_policy(enforce_budget_policy(15.0));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "under-budget command must dispatch: {outcome:?}"
    );
    assert!(store
        .events_for(&instance.id)
        .await?
        .iter()
        .all(|event| event.event_type != "BudgetShadowDecision"));
    Ok(())
}

#[tokio::test]
async fn budget_gate_unlimited_skips_check() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_command(&store, 504).await?;
    store
        .upsert_runtime_usage(&budget_usage_upsert(&instance.id, cost_usd_to_micros(500.0)?))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    )
    .with_budget_policy(RuntimeBudgetPolicy {
        unlimited: true,
        ..RuntimeBudgetPolicy::default()
    });
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "unlimited policy must dispatch regardless of spend: {outcome:?}"
    );
    assert!(store
        .events_for(&instance.id)
        .await?
        .iter()
        .all(|event| event.event_type != "BudgetShadowDecision"));
    Ok(())
}
