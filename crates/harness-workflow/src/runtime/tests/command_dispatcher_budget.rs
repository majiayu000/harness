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

fn profile_usage_upsert(
    workflow_id: &str,
    runtime_profile: &str,
    cost_usd_micros: u64,
) -> RuntimeUsageUpsert {
    RuntimeUsageUpsert {
        runtime_profile: runtime_profile.to_string(),
        ..budget_usage_upsert(workflow_id, cost_usd_micros)
    }
}

fn daily_cap_policy(cap_usd: f64, enforcement: RuntimeBudgetEnforcement) -> RuntimeBudgetPolicy {
    RuntimeBudgetPolicy {
        enforcement,
        daily_profile_cap_usd: Some(cap_usd),
        ..RuntimeBudgetPolicy::default()
    }
}

fn enforce_budget_policy(budget_usd: f64) -> RuntimeBudgetPolicy {
    RuntimeBudgetPolicy {
        default_workflow_budget_usd: budget_usd,
        enforcement: RuntimeBudgetEnforcement::Enforce,
        unlimited: false,
        daily_profile_cap_usd: None,
        daily_throttle_ratio: 0.8,
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
    )
    .with_budget_policy(RuntimeBudgetPolicy {
        enforcement: RuntimeBudgetEnforcement::Shadow,
        ..RuntimeBudgetPolicy::default()
    });
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
async fn budget_gate_default_enforces_budget_barrier() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, command_id) = issue_with_pending_command(&store, 520).await?;
    store
        .upsert_runtime_usage(&budget_usage_upsert(&instance.id, cost_usd_to_micros(16.0)?))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    );
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    let barrier = match outcome {
        CommandDispatchOutcome::Deferred {
            command_id: id,
            barrier,
        } => {
            assert_eq!(id, command_id);
            barrier
        }
        other => panic!("default budget policy must enforce over-budget commands: {other:?}"),
    };
    assert_eq!(
        barrier.reason_code,
        DispatchBarrierReasonCode::WorkflowBudgetExhausted
    );
    assert_eq!(
        store.commands_for(&instance.id).await?[0].status,
        WorkflowCommandStatus::Deferred
    );
    let events = store.events_for(&instance.id).await?;
    let deferred = events
        .iter()
        .find(|event| event.event_type == "WorkflowRuntimeDispatchDeferred")
        .expect("default enforcement records a dispatch-deferred audit event");
    assert_eq!(deferred.source, "workflow_runtime_command_dispatcher");
    assert_eq!(
        deferred.event["dispatch_barrier"]["reason_code"],
        "workflow_budget_exhausted"
    );
    assert!(events
        .iter()
        .all(|event| event.event_type != "BudgetShadowDecision"));
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

#[tokio::test]
async fn daily_profile_cap_shadow_records_event_and_dispatches() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_command(&store, 505).await?;
    // The cap aggregates today's spend across workflows for the profile the
    // dispatcher selects ("codex-high" here), not just this workflow.
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            "other-workflow",
            "codex-high",
            cost_usd_to_micros(9.0)?,
        ))
        .await?;
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            &instance.id,
            "codex-high",
            cost_usd_to_micros(2.0)?,
        ))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    )
    .with_budget_policy(daily_cap_policy(10.0, RuntimeBudgetEnforcement::Shadow));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "shadow mode must dispatch despite the breached daily cap: {outcome:?}"
    );
    let events = store.events_for(&instance.id).await?;
    let shadow = events
        .iter()
        .find(|event| event.event_type == "BudgetShadowDecision")
        .expect("shadow mode records a BudgetShadowDecision event");
    assert_eq!(shadow.event["decision"], "would_defer");
    assert_eq!(
        shadow.event["barrier_reason_code"],
        "profile_daily_cap_reached"
    );
    assert_eq!(shadow.event["runtime_profile"], "codex-high");
    assert_eq!(shadow.event["daily_profile_cap_usd"], 10.0);
    assert_eq!(shadow.event["profile_spent_usd_today"], 11.0);
    Ok(())
}

#[tokio::test]
async fn daily_profile_cap_enforce_defers_and_ignores_other_profiles() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (_instance, command_id) = issue_with_pending_command(&store, 506).await?;
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            "other-workflow",
            "codex-high",
            cost_usd_to_micros(10.0)?,
        ))
        .await?;
    // Spend on a different profile must not count toward this cap.
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            "unrelated-workflow",
            "codex-low",
            cost_usd_to_micros(100.0)?,
        ))
        .await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc),
    )
    .with_budget_policy(daily_cap_policy(10.0, RuntimeBudgetEnforcement::Enforce));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    let barrier = match outcome {
        CommandDispatchOutcome::Deferred {
            command_id: id,
            barrier,
        } => {
            assert_eq!(id, command_id);
            barrier
        }
        other => panic!("enforce mode must defer past the daily cap: {other:?}"),
    };
    assert_eq!(
        barrier.reason_code,
        DispatchBarrierReasonCode::ProfileDailyCapReached
    );
    assert!(barrier.reason.contains("codex-high"), "{}", barrier.reason);

    // Under the cap on a fresh profile, the same policy dispatches.
    let (under_instance, _) = issue_with_pending_command(&store, 507).await?;
    let under_dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-fresh", RuntimeKind::CodexJsonrpc),
    )
    .with_budget_policy(daily_cap_policy(10.0, RuntimeBudgetEnforcement::Enforce));
    let under_outcome = under_dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");
    assert!(
        matches!(under_outcome, CommandDispatchOutcome::Enqueued { .. }),
        "fresh profile under the cap must dispatch: {under_outcome:?}"
    );
    assert!(store
        .events_for(&under_instance.id)
        .await?
        .iter()
        .all(|event| event.event_type != "BudgetShadowDecision"));
    Ok(())
}

// Throttle band (GH-1770 §4.1): inside the band a profile yields the dispatch
// slot to a profile under its own threshold, but is never starved when it is
// the only claimable work.

fn throttle_policy(cap_usd: f64, enforcement: RuntimeBudgetEnforcement) -> RuntimeBudgetPolicy {
    RuntimeBudgetPolicy {
        enforcement,
        daily_profile_cap_usd: Some(cap_usd),
        daily_throttle_ratio: 0.8,
        ..RuntimeBudgetPolicy::default()
    }
}

/// Selector where `replan_issue` keeps the (throttled) default profile and
/// `implement_issue` resolves to a second, cheaper profile.
fn throttle_profile_selector() -> RuntimeProfileSelector {
    RuntimeProfileSelector::new(RuntimeProfile::new("codex-high", RuntimeKind::CodexJsonrpc))
        .with_activity_profile(
            "implement_issue",
            RuntimeProfile::new("codex-low", RuntimeKind::CodexJsonrpc),
        )
}

async fn issue_with_pending_activity(
    store: &WorkflowRuntimeStore,
    issue_number: u64,
    activity: &str,
) -> anyhow::Result<(WorkflowInstance, String)> {
    let instance = project_issue_instance("/project-a", issue_number, "replanning");
    store.force_upsert_lifecycle_state_for_test(&instance).await?;
    let command = WorkflowCommand::enqueue_activity(
        activity,
        format!("issue-{issue_number}-{activity}-throttle"),
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    Ok((instance, command_id))
}

#[tokio::test]
async fn throttle_band_yields_to_a_profile_under_its_threshold() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    // Claimed first (oldest): the throttled profile's command.
    let (throttled_instance, throttled_command_id) =
        issue_with_pending_activity(&store, 511, "replan_issue").await?;
    issue_with_pending_activity(&store, 512, "implement_issue").await?;
    // 8.50 of a 10.00 cap: inside the band (threshold 8.00), under the cap.
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            &throttled_instance.id,
            "codex-high",
            cost_usd_to_micros(8.5)?,
        ))
        .await?;

    let dispatcher =
        RuntimeCommandDispatcher::with_profile_selector(&store, throttle_profile_selector())
            .with_budget_policy(throttle_policy(10.0, RuntimeBudgetEnforcement::Enforce));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    let barrier = match outcome {
        CommandDispatchOutcome::Deferred {
            command_id: id,
            barrier,
        } => {
            assert_eq!(id, throttled_command_id);
            barrier
        }
        other => panic!("throttled profile must yield the slot: {other:?}"),
    };
    assert_eq!(
        barrier.reason_code,
        DispatchBarrierReasonCode::ProfileDailyThrottled
    );
    assert!(barrier.reason.contains("codex-low"), "{}", barrier.reason);
    Ok(())
}

#[tokio::test]
async fn throttle_band_dispatches_when_it_is_the_only_claimable_work() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_activity(&store, 513, "replan_issue").await?;
    // A second command on the same throttled profile is not an alternative:
    // yielding to it would leave both deferred forever.
    issue_with_pending_activity(&store, 514, "replan_issue").await?;
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            &instance.id,
            "codex-high",
            cost_usd_to_micros(8.5)?,
        ))
        .await?;

    let dispatcher =
        RuntimeCommandDispatcher::with_profile_selector(&store, throttle_profile_selector())
            .with_budget_policy(throttle_policy(10.0, RuntimeBudgetEnforcement::Enforce));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "a throttled profile with no better alternative must still run: {outcome:?}"
    );
    Ok(())
}

#[tokio::test]
async fn throttle_band_shadow_records_the_decision_and_dispatches() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_activity(&store, 515, "replan_issue").await?;
    issue_with_pending_activity(&store, 516, "implement_issue").await?;
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            &instance.id,
            "codex-high",
            cost_usd_to_micros(8.5)?,
        ))
        .await?;

    let dispatcher =
        RuntimeCommandDispatcher::with_profile_selector(&store, throttle_profile_selector())
            .with_budget_policy(throttle_policy(10.0, RuntimeBudgetEnforcement::Shadow));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "shadow mode must dispatch inside the throttle band: {outcome:?}"
    );
    let events = store.events_for(&instance.id).await?;
    let shadow = events
        .iter()
        .find(|event| event.event_type == "BudgetShadowDecision")
        .expect("shadow mode records a BudgetShadowDecision event");
    assert_eq!(shadow.event["barrier_reason_code"], "profile_daily_throttled");
    assert_eq!(shadow.event["daily_throttle_threshold_usd"], 8.0);
    assert_eq!(shadow.event["yielded_to_runtime_profile"], "codex-low");
    Ok(())
}

#[tokio::test]
async fn throttle_band_is_inert_below_the_threshold() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (instance, _) = issue_with_pending_activity(&store, 517, "replan_issue").await?;
    issue_with_pending_activity(&store, 518, "implement_issue").await?;
    store
        .upsert_runtime_usage(&profile_usage_upsert(
            &instance.id,
            "codex-high",
            cost_usd_to_micros(7.99)?,
        ))
        .await?;

    let dispatcher =
        RuntimeCommandDispatcher::with_profile_selector(&store, throttle_profile_selector())
            .with_budget_policy(throttle_policy(10.0, RuntimeBudgetEnforcement::Enforce));
    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");

    assert!(
        matches!(outcome, CommandDispatchOutcome::Enqueued { .. }),
        "below the threshold the band must not engage: {outcome:?}"
    );
    assert!(store
        .events_for(&instance.id)
        .await?
        .iter()
        .all(|event| event.event_type != "BudgetShadowDecision"));
    Ok(())
}
