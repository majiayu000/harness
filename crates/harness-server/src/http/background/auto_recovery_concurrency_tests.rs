#[tokio::test]
async fn auto_recovery_recover_race_records_superseded_without_consuming_attempt(
) -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-race-").await?;
    let github = test_github_config(3);
    let stale_snapshot = seed_stopped_instance(
        &store,
        "ar-race-1",
        "blocked",
        transient_blocked_data("episode-1"),
    )
    .await?;

    // Concurrent manual operator unblock wins between listing and the recheck.
    let manual = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &stale_snapshot.id,
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "manual unblock during pending recheck",
            actor: "operator",
            target_state: None,
            evidence: &[],
        })
        .await?;
    assert!(matches!(
        manual,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));

    let outcome = process_instance(
        &store,
        &github,
        &crate::alerting::AlertHandle::disabled(),
        &stale_snapshot,
        Utc::now(),
    )
    .await?;
    assert_eq!(outcome, InstanceOutcome::Superseded);

    let after = refreshed(&store, &stale_snapshot.id).await?;
    assert_eq!(
        after.state, "implementing",
        "exactly one recovery transition"
    );
    assert!(
        instance_attempt_state(&after).is_none(),
        "no attempt consumed"
    );
    let events = store.events_for(&stale_snapshot.id).await?;
    let outcome_event = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_OUTCOME_EVENT)
        .expect("superseded outcome recorded");
    assert_eq!(outcome_event.event["outcome"], json!("superseded"));
    assert_eq!(
        event_types(&events)
            .iter()
            .filter(|event_type| **event_type == "WorkflowRuntimeUnblocked")
            .count(),
        1,
        "no double recovery"
    );
    Ok(())
}

// -- B-013: manual recovery stays available while a recheck is pending ------

#[tokio::test]
async fn auto_recovery_manual_unblock_wins_during_pending_backoff() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-manual-").await?;

    let mut data = transient_blocked_data("episode-1");
    data["auto_recovery"] = json!({
        "episode_event_id": "episode-1",
        "attempts": 1,
        "last_outcome": "recheck_failed",
        "next_attempt_at": Utc::now() + ChronoDuration::hours(1),
    });
    let instance = seed_stopped_instance(&store, "ar-manual-1", "blocked", data).await?;

    let outcome = store
        .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
            workflow_id: &instance.id,
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "manual unblock while backoff pending",
            actor: "operator",
            target_state: None,
            evidence: &[],
        })
        .await?;
    assert!(matches!(
        outcome,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));
    let after = refreshed(&store, &instance.id).await?;
    assert_eq!(after.state, "implementing");
    assert!(
        instance_attempt_state(&after).is_none(),
        "manual recovery clears auto-recovery attempt state"
    );
    Ok(())
}

// -- Terminal recheck outcomes stop scheduling immediately -------------------

#[tokio::test]
async fn auto_recovery_terminal_recheck_outcome_stops_scheduling() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-recheck-").await?;
    let github = test_github_config(3);

    // Structured transient stop pointing at a runtime job that no longer
    // exists: recovery cannot determine a supported stopped activity. That
    // cannot heal within the episode, so retrying would be futile.
    let mut data = transient_blocked_data("episode-1");
    data["last_stop"]["runtime_job_id"] = json!("job-missing");
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "blocked",
        WorkflowSubject::new("issue", "issue:ar-recheck-1"),
    )
    .with_id("ar-recheck-1".to_string())
    .with_data(data);
    store.upsert_instance(&workflow).await?;

    let now = Utc::now();
    let tick = run_auto_recovery_tick(
        &store,
        &github,
        &crate::alerting::AlertHandle::disabled(),
        now,
    )
    .await?;
    assert_eq!(tick.recheck_failed, 1);

    let after = refreshed(&store, &workflow.id).await?;
    assert_eq!(
        after.state, "blocked",
        "no state transition on failed recheck"
    );
    let attempt_state = instance_attempt_state(&after).expect("attempt state persists");
    assert_eq!(attempt_state.attempts, 1);
    assert_eq!(
        attempt_state.last_outcome.as_deref(),
        Some("recheck_failed")
    );
    assert!(
        attempt_state.exhausted,
        "terminal recheck marks the episode exhausted immediately"
    );
    assert!(attempt_state.next_attempt_at.is_none());
    let events = store.events_for(&workflow.id).await?;
    let outcome = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_OUTCOME_EVENT)
        .expect("recheck_failed outcome recorded");
    assert_eq!(outcome.event["outcome"], json!("recheck_failed"));
    let exhausted = events
        .iter()
        .find(|event| event.event_type == AUTO_RECOVERY_EXHAUSTED_EVENT)
        .expect("terminal recheck emits the exhaustion event");
    assert!(exhausted.event["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("no supported stopped activity")));

    // Zero further recheck attempts on subsequent ticks, even far in the
    // future: the episode is terminal until a manual operator action.
    let event_count = events.len();
    for offset_hours in [1, 24] {
        let tick = run_auto_recovery_tick(
            &store,
            &github,
            &crate::alerting::AlertHandle::disabled(),
            now + ChronoDuration::hours(offset_hours),
        )
        .await?;
        assert_eq!(tick, AutoRecoveryTick::default());
    }
    assert_eq!(
        store.events_for(&workflow.id).await?.len(),
        event_count,
        "no further attempt or outcome events"
    );
    Ok(())
}

// -- Starvation regression: ineligible backlog cannot clog the scan window --

#[tokio::test]
async fn auto_recovery_scan_is_not_starved_by_ineligible_backlog() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let (_dir, store) = open_test_store("harness-test-ar-starve-").await?;
    let github = test_github_config(3);

    // More than a full scan window of older ineligible rows: terminal
    // classification in the opted-in repo. Their updated_at never changes,
    // so a naive updated_at ASC window would always be full of them.
    for index in 0..AUTO_RECOVERY_SCAN_LIMIT + 10 {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "blocked",
            WorkflowSubject::new("issue", format!("issue:ar-starve-{index}")),
        )
        .with_id(format!("ar-starve-{index}"))
        .with_data(json!({
            "repo": TEST_REPO,
            "stop_reason_code": "maintainer_input_required",
            "reason_class": "terminal",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "event_id": format!("episode-{index}"),
            },
        }));
        store.upsert_instance(&workflow).await?;
    }
    // The eligible instance arrives last (newest updated_at).
    let eligible = seed_stopped_instance(
        &store,
        "ar-starve-eligible",
        "blocked",
        transient_blocked_data("episode-eligible"),
    )
    .await?;

    let tick = run_auto_recovery_tick(
        &store,
        &github,
        &crate::alerting::AlertHandle::disabled(),
        Utc::now(),
    )
    .await?;
    assert_eq!(
        tick.recovered, 1,
        "the newest eligible instance must not be starved by the backlog"
    );
    assert_eq!(refreshed(&store, &eligible.id).await?.state, "implementing");
    Ok(())
}
