async fn reclaim_claim_with_same_owner(
    store: &WorkflowRuntimeStore,
    command_id: &str,
    owner: &str,
) -> anyhow::Result<WorkflowCommandRecord> {
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_lease_expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(command_id)
    .execute(store.pool())
    .await?;
    Ok(store
        .claim_pending_commands(owner, Utc::now() + Duration::minutes(1), 10)
        .await?
        .into_iter()
        .find(|record| record.id == command_id)
        .expect("expired claim should be reclaimed by the same dispatcher"))
}

#[tokio::test]
async fn terminal_dispatcher_fast_path_rejects_reclaimed_generation() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (mut workflow, command_id, old_claim) =
        claimed_deferred_test_command(&store, "terminal-reclaimed", "reused-owner").await?;
    let current_claim = reclaim_claim_with_same_owner(&store, &command_id, "reused-owner").await?;
    assert_eq!(
        current_claim.dispatch_claim_generation,
        old_claim.dispatch_claim_generation + 1
    );
    workflow.state = "cancelled".to_string();
    store.upsert_instance(&workflow).await?;

    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
    )
    .with_dispatcher_id("reused-owner");
    let stale_outcome = dispatcher.dispatch_command(old_claim).await?;
    assert!(matches!(stale_outcome, CommandDispatchOutcome::Skipped { .. }));
    let after_stale = store.get_command(&command_id).await?.expect("command exists");
    assert_eq!(after_stale.status, WorkflowCommandStatus::Dispatching);
    assert_eq!(
        after_stale.dispatch_claim_generation,
        current_claim.dispatch_claim_generation
    );
    assert_eq!(after_stale.dispatch_owner.as_deref(), Some("reused-owner"));

    let terminal_outcome = dispatcher.dispatch_command(current_claim).await?;
    assert!(matches!(
        terminal_outcome,
        CommandDispatchOutcome::Skipped { .. }
    ));
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("command exists")
            .status,
        WorkflowCommandStatus::Cancelled
    );
    assert!(store.runtime_jobs_for_command(&command_id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn same_owner_newer_generation_rejects_stale_deferral() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (workflow, command_id, old_claim) =
        claimed_deferred_test_command(&store, "same-owner-defer", "reused-owner").await?;
    let current_claim = reclaim_claim_with_same_owner(&store, &command_id, "reused-owner").await?;

    assert_eq!(
        store
            .defer_claimed_command_if_owned(
                &command_id,
                "reused-owner",
                old_claim.dispatch_claim_generation,
                test_dispatch_barrier("stale same-owner generation"),
                Utc::now(),
                DispatchBackoffPolicy::default(),
            )
            .await?,
        DeferClaimedCommandOutcome::StaleClaim
    );
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 0);
    assert!(matches!(
        store
            .defer_claimed_command_if_owned(
                &command_id,
                "reused-owner",
                current_claim.dispatch_claim_generation,
                test_dispatch_barrier("current same-owner generation"),
                Utc::now(),
                DispatchBackoffPolicy::default(),
            )
            .await?,
        DeferClaimedCommandOutcome::Deferred(_)
    ));
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 1);
    Ok(())
}

#[tokio::test]
async fn stale_generation_claimed_enqueue_is_write_free() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (_, command_id, old_claim) =
        claimed_deferred_test_command(&store, "stale-enqueue", "reused-owner").await?;
    let current_claim = reclaim_claim_with_same_owner(&store, &command_id, "reused-owner").await?;
    let input = json!({"activity": "implement_issue"});

    assert_eq!(
        store
            .enqueue_runtime_job_for_claimed_command(
                &command_id,
                DispatchClaim {
                    owner: "reused-owner",
                    generation: old_claim.dispatch_claim_generation,
                },
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                input.clone(),
                None,
            )
            .await?,
        RuntimeJobEnqueueOutcome::StaleClaim
    );
    assert!(store.runtime_jobs_for_command(&command_id).await?.is_empty());
    let current = store
        .enqueue_runtime_job_for_claimed_command(
            &command_id,
            DispatchClaim {
                owner: "reused-owner",
                generation: current_claim.dispatch_claim_generation,
            },
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            input,
            None,
        )
        .await?;
    assert!(matches!(current, RuntimeJobEnqueueOutcome::Enqueued(_)));
    assert_eq!(store.runtime_jobs_for_command(&command_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn terminal_workflow_wins_claimed_enqueue() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (mut workflow, command_id, claim) =
        claimed_deferred_test_command(&store, "terminal-enqueue", "terminal-owner").await?;
    workflow.state = "cancelled".to_string();
    store.upsert_instance(&workflow).await?;

    assert_eq!(
        store
            .enqueue_runtime_job_for_claimed_command(
                &command_id,
                DispatchClaim {
                    owner: "terminal-owner",
                    generation: claim.dispatch_claim_generation,
                },
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({"activity": "implement_issue"}),
                None,
            )
            .await?,
        RuntimeJobEnqueueOutcome::WorkflowTerminal {
            status: WorkflowCommandStatus::Cancelled
        }
    );
    assert!(store.runtime_jobs_for_command(&command_id).await?.is_empty());
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("command exists")
            .status,
        WorkflowCommandStatus::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn deferred_command_rejects_incomplete_or_mismatched_evidence() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (_, command_id, claim) =
        claimed_deferred_test_command(&store, "invalid-evidence", "evidence-owner").await?;
    assert!(matches!(
        store
            .defer_claimed_command_if_owned(
                &command_id,
                "evidence-owner",
                claim.dispatch_claim_generation,
                test_dispatch_barrier("valid evidence baseline"),
                Utc::now(),
                DispatchBackoffPolicy::default(),
            )
            .await?,
        DeferClaimedCommandOutcome::Deferred(_)
    ));
    let valid = store.get_command(&command_id).await?.expect("command exists");
    let due = valid.dispatch_not_before.expect("valid due evidence");
    let barrier = valid.dispatch_barrier.expect("valid barrier evidence");

    let mut wrong_command = serde_json::to_value(&barrier)?;
    wrong_command["command_id"] = json!("different-command");
    let mut wrong_workflow = serde_json::to_value(&barrier)?;
    wrong_workflow["workflow_id"] = json!("different-workflow");
    let mut wrong_owner = serde_json::to_value(&barrier)?;
    wrong_owner["dispatch_owner"] = json!("");
    let mut wrong_due = serde_json::to_value(&barrier)?;
    wrong_due["next_dispatch_at"] = json!((due + Duration::seconds(1)).to_rfc3339());
    let mut wrong_attempt = serde_json::to_value(&barrier)?;
    wrong_attempt["attempt"] = json!(barrier.attempt + 1);
    let mut wrong_generation = serde_json::to_value(&barrier)?;
    wrong_generation["claim_generation"] = json!(barrier.claim_generation + 1);
    let valid_json = serde_json::to_string(&barrier)?;
    let cases = vec![
        ("null barrier", Some(due), 1_i64, 1_i64, None, None),
        ("empty barrier", Some(due), 1, 1, Some("{}".to_string()), None),
        ("null schedule", None, 1, 1, Some(valid_json.clone()), None),
        ("zero attempt", Some(due), 0, 1, Some(valid_json.clone()), None),
        ("zero generation", Some(due), 1, 0, Some(valid_json.clone()), None),
        ("wrong command", Some(due), 1, 1, Some(wrong_command.to_string()), None),
        ("wrong workflow", Some(due), 1, 1, Some(wrong_workflow.to_string()), None),
        ("empty owner", Some(due), 1, 1, Some(wrong_owner.to_string()), None),
        ("wrong due", Some(due), 1, 1, Some(wrong_due.to_string()), None),
        ("wrong attempt", Some(due), 1, 1, Some(wrong_attempt.to_string()), None),
        ("wrong generation", Some(due), 1, 1, Some(wrong_generation.to_string()), None),
        (
            "unreleased owner",
            Some(due),
            1,
            1,
            Some(valid_json.clone()),
            Some("unexpected-owner"),
        ),
    ];
    for (name, case_due, attempt, generation, case_barrier, owner) in cases {
        sqlx::query(
            "UPDATE workflow_commands
             SET dispatch_not_before = $2, dispatch_attempt_count = $3,
                 dispatch_claim_generation = $4, dispatch_barrier = $5::jsonb,
                 dispatch_owner = $6, dispatch_lease_expires_at = NULL
             WHERE id = $1",
        )
        .bind(&command_id)
        .bind(case_due)
        .bind(attempt)
        .bind(generation)
        .bind(case_barrier)
        .bind(owner)
        .execute(store.pool())
        .await?;
        assert!(
            store.get_command(&command_id).await.is_err(),
            "{name} must fail closed"
        );
    }
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_not_before = CURRENT_TIMESTAMP - INTERVAL '1 second',
             dispatch_attempt_count = 1, dispatch_claim_generation = 1,
             dispatch_barrier = $2::jsonb, dispatch_owner = NULL,
             dispatch_lease_expires_at = NULL
         WHERE id = $1",
    )
    .bind(&command_id)
    .bind(wrong_command.to_string())
    .execute(store.pool())
    .await?;
    assert!(store
        .defer_claimed_command_if_owned(
            &command_id,
            "evidence-owner",
            1,
            test_dispatch_barrier("must reject corrupt replay"),
            Utc::now(),
            DispatchBackoffPolicy::default(),
        )
        .await
        .is_err());
    assert!(store
        .claim_pending_commands("must-not-run", Utc::now() + Duration::minutes(1), 10)
        .await?
        .is_empty());
    let (status, generation): (String, i64) = sqlx::query_as(
        "SELECT status, dispatch_claim_generation FROM workflow_commands WHERE id = $1",
    )
    .bind(&command_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(status, "deferred");
    assert_eq!(generation, 1);
    assert!(store.runtime_jobs_for_command(&command_id).await?.is_empty());
    Ok(())
}

#[test]
fn dispatch_backoff_rejects_out_of_range_chrono_seconds() {
    let max = u64::try_from(chrono::Duration::MAX.num_seconds()).expect("positive Chrono max");
    assert!(DispatchBackoffPolicy::from_seconds(max, max).is_ok());
    assert!(DispatchBackoffPolicy::from_seconds(max + 1, max + 1).is_err());
    assert!(DispatchBackoffPolicy::from_seconds(u64::MAX, u64::MAX).is_err());
}
