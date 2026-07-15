use super::*;

// -- B-005: automated recovery re-runs the manual recovery path -------------

#[tokio::test]
async fn auto_recovery_transition_matches_manual_unblock_except_actor() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-parity-").await?;
    let github = test_github_config(3);

    let auto = seed_stopped_instance(
        &store,
        "ar-parity-auto",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;
    let mut manual_data = transient_blocked_data("episode-1");
    manual_data["repo"] = json!("owner/manual");
    let manual = seed_stopped_instance(&store, "ar-parity-manual", "blocked", manual_data).await?;

    let tick = run_auto_recovery_tick(
        &store,
        &github,
        &crate::alerting::AlertHandle::disabled(),
        Utc::now(),
    )
    .await?;
    assert_eq!(tick.recovered, 1);
    let manual_outcome = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &manual.id,
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator resolved the rate limit",
            actor: "operator",
            target_state: None,
        })
        .await?;
    assert!(matches!(
        manual_outcome,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));

    let auto_after = refreshed(&store, &auto.id).await?;
    let manual_after = refreshed(&store, &manual.id).await?;
    assert_eq!(auto_after.state, manual_after.state);
    assert_eq!(auto_after.data.get("last_stop"), auto.data.get("last_stop"));
    assert_eq!(
        manual_after.data.get("last_stop"),
        manual.data.get("last_stop")
    );
    assert_eq!(
        auto_after.data.pointer("/last_operator_recovery/actor"),
        Some(&json!(AUTO_RECOVERY_ACTOR))
    );
    assert_eq!(
        manual_after.data.pointer("/last_operator_recovery/actor"),
        Some(&json!("operator"))
    );
    let command_activity = |commands: Vec<harness_workflow::runtime::WorkflowCommandRecord>| {
        commands
            .into_iter()
            .filter(|record| {
                record.status == harness_workflow::runtime::WorkflowCommandStatus::Pending
            })
            .map(|record| record.command.activity_name().map(str::to_string))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        command_activity(store.commands_for(&auto.id).await?),
        command_activity(store.commands_for(&manual.id).await?),
    );
    Ok(())
}

// -- B-006: bounded attempts persisted in instance data ---------------------
// -- B-010: exhaustion emits one event, instance stays visible ---------------

#[tokio::test]
async fn auto_recovery_state_attempt_bound_survives_restart_and_exhaustion_is_idempotent(
) -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-exhaust-").await?;
    let github = test_github_config(2);

    let mut data = transient_blocked_data("episode-1");
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 2,
        "last_outcome": "recheck_failed",
    });
    let instance = seed_stopped_instance(&store, "ar-exhaust-1", "blocked", data).await?;
    let events =
        std::sync::Arc::new(harness_observe::event_store::EventStore::new(_dir.path()).await?);
    let transport =
        std::sync::Arc::new(crate::alerting::adapters::test_support::MockTransport::ok());
    let alerting_config = harness_core::config::alerting::AlertingConfig {
        enabled: true,
        event_classes: vec![
            harness_core::alert::AlertClass::WorkflowBlocked,
            harness_core::alert::AlertClass::WorkflowFailed,
        ],
        dedup_cooldown_secs: 0,
        queue_capacity: 8,
        shutdown_flush_secs: 5,
        channels: vec![harness_core::config::alerting::AlertChannelConfig {
            name: "ops".into(),
            kind: harness_core::config::alerting::AlertChannelKind::Webhook,
            url: Some("https://example.invalid/hook".into()),
            receive_id: None,
            max_attempts: 2,
            backoff_base_ms: 1,
        }],
        heartbeat: Default::default(),
    };
    let alerts = crate::alerting::spawn_alerting(&alerting_config, None, events, transport.clone());

    let first = run_auto_recovery_tick(&store, &github, &alerts, Utc::now()).await?;
    assert_eq!(first.exhausted_emitted, 1);
    assert_eq!(first.recovered, 0);
    let second = run_auto_recovery_tick(&store, &github, &alerts, Utc::now()).await?;
    assert_eq!(second.exhausted_emitted, 0);
    assert_eq!(second, AutoRecoveryTick::default());

    for _ in 0..200 {
        if transport.call_count() >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(transport.call_count(), 1, "exactly one exhaustion alert");
    {
        let calls = transport.calls.lock().unwrap();
        let (_, _, body) = &calls[0];
        let text = body.to_string();
        assert!(
            text.contains("auto-recovery exhausted after 2 attempts"),
            "{text}"
        );
        assert!(text.contains("rate_limited"), "{text}");
    }

    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(
        after.state, "blocked",
        "instance stays stopped for the operator"
    );
    let attempt_state = instance_attempt_state(&after).expect("attempt state persists");
    assert_eq!(attempt_state.attempts, 2);
    assert!(attempt_state.exhausted);
    let events = store.events_for(&instance.id).await?;
    assert_eq!(
        event_types(&events)
            .iter()
            .filter(|event_type| **event_type == AUTO_RECOVERY_EXHAUSTED_EVENT)
            .count(),
        1,
        "exactly one AutoRecoveryExhausted per episode"
    );
    assert_eq!(
        after.data.pointer("/last_stop/stop_reason_code"),
        Some(&json!("rate_limited"))
    );
    Ok(())
}
