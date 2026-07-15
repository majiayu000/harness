fn test_dispatch_barrier(reason: &str) -> DispatchBarrierInput {
    DispatchBarrierInput::new(
        DispatchBarrierReasonCode::RuntimePolicyDisabled,
        reason,
        "project-deferred-dispatch",
    )
}

async fn claimed_deferred_test_command(
    store: &WorkflowRuntimeStore,
    suffix: &str,
    owner: &str,
) -> anyhow::Result<(WorkflowInstance, String, WorkflowCommandRecord)> {
    let workflow = issue_instance("implementing").with_id(format!("deferred-{suffix}"));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity(
        "implement_issue",
        format!("deferred-command-{suffix}"),
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let claimed = store
        .claim_pending_commands(owner, Utc::now() + Duration::minutes(1), 10)
        .await?;
    let record = claimed
        .into_iter()
        .find(|record| record.id == command_id)
        .expect("test command should be claimed");
    Ok((workflow, command_id, record))
}

async fn force_deferred_due(store: &WorkflowRuntimeStore, command_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_not_before = CURRENT_TIMESTAMP - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(command_id)
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn deferred_event_count(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<usize> {
    Ok(store
        .events_for(workflow_id)
        .await?
        .into_iter()
        .filter(|event| event.event_type == "WorkflowRuntimeDispatchDeferred")
        .count())
}

#[tokio::test]
async fn defer_claimed_command_is_atomic() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (workflow, command_id, claim) =
        claimed_deferred_test_command(&store, "atomic", "dispatcher-atomic").await?;

    sqlx::query(
        "CREATE FUNCTION reject_dispatch_deferred_event() RETURNS trigger AS $$
         BEGIN
           IF NEW.event_type = 'WorkflowRuntimeDispatchDeferred' THEN
             RAISE EXCEPTION 'forced deferred event failure';
           END IF;
           RETURN NEW;
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(store.pool())
    .await?;
    sqlx::query(
        "CREATE TRIGGER reject_dispatch_deferred_event
         BEFORE INSERT ON workflow_events
         FOR EACH ROW EXECUTE FUNCTION reject_dispatch_deferred_event()",
    )
    .execute(store.pool())
    .await?;

    let error = store
        .defer_claimed_command_if_owned(
            &command_id,
            "dispatcher-atomic",
            claim.dispatch_claim_generation,
            test_dispatch_barrier("policy disabled for atomic rollback"),
            Utc::now(),
            DispatchBackoffPolicy::from_seconds(5, 20)?,
        )
        .await
        .expect_err("event insertion failure must abort the entire deferral transaction");
    assert!(format!("{error:#}").contains("forced deferred event failure"));
    let persisted = store.get_command(&command_id).await?.expect("command exists");
    assert_eq!(persisted.status, WorkflowCommandStatus::Dispatching);
    assert_eq!(persisted.dispatch_attempt_count, 0);
    assert!(persisted.dispatch_not_before.is_none());
    assert!(persisted.dispatch_barrier.is_none());
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn defer_claimed_command_rejects_stale_owner() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (workflow, command_id, claim) =
        claimed_deferred_test_command(&store, "stale", "dispatcher-current").await?;

    for (owner, generation) in [
        ("dispatcher-stale", claim.dispatch_claim_generation),
        ("dispatcher-current", claim.dispatch_claim_generation + 1),
    ] {
        assert_eq!(
            store
                .defer_claimed_command_if_owned(
                    &command_id,
                    owner,
                    generation,
                    test_dispatch_barrier("stale claim must not mutate"),
                    Utc::now(),
                    DispatchBackoffPolicy::default(),
                )
                .await?,
            DeferClaimedCommandOutcome::StaleClaim
        );
    }
    let persisted = store.get_command(&command_id).await?.expect("command exists");
    assert_eq!(persisted.status, WorkflowCommandStatus::Dispatching);
    assert_eq!(persisted.dispatch_claim_generation, claim.dispatch_claim_generation);
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn deferred_command_dispatcher_race_has_one_winner() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (workflow, command_id, claim) =
        claimed_deferred_test_command(&store, "race", "dispatcher-race").await?;
    let generation = claim.dispatch_claim_generation;
    let now = Utc::now();
    let backoff = DispatchBackoffPolicy::from_seconds(5, 20)?;
    let first = store.defer_claimed_command_if_owned(
        &command_id,
        "dispatcher-race",
        generation,
        test_dispatch_barrier("race barrier"),
        now,
        backoff,
    );
    let second = store.defer_claimed_command_if_owned(
        &command_id,
        "dispatcher-race",
        generation,
        test_dispatch_barrier("race barrier"),
        now,
        backoff,
    );
    let (first, second) = tokio::join!(first, second);
    let outcomes = [first?, second?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DeferClaimedCommandOutcome::Deferred(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DeferClaimedCommandOutcome::AlreadyDeferred(_)))
            .count(),
        1
    );
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 1);
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("command exists")
            .dispatch_attempt_count,
        1
    );
    Ok(())
}

#[tokio::test]
async fn terminal_workflow_wins_dispatch_race() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (mut workflow, command_id, claim) =
        claimed_deferred_test_command(&store, "terminal", "dispatcher-terminal").await?;
    workflow.state = "cancelled".to_string();
    store.upsert_instance(&workflow).await?;

    assert_eq!(
        store
            .defer_claimed_command_if_owned(
                &command_id,
                "dispatcher-terminal",
                claim.dispatch_claim_generation,
                test_dispatch_barrier("terminal workflow"),
                Utc::now(),
                DispatchBackoffPolicy::default(),
            )
            .await?,
        DeferClaimedCommandOutcome::WorkflowTerminal {
            status: WorkflowCommandStatus::Cancelled
        }
    );
    let persisted = store.get_command(&command_id).await?.expect("command exists");
    assert_eq!(persisted.status, WorkflowCommandStatus::Cancelled);
    assert!(persisted.dispatch_barrier.is_none());
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 0);
    assert!(store.runtime_jobs_for_command(&command_id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn failed_defer_reclaims_after_lease_expiry() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (workflow, command_id, first_claim) =
        claimed_deferred_test_command(&store, "reclaim", "dispatcher-first").await?;
    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_lease_expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(&command_id)
    .execute(store.pool())
    .await?;
    let reclaimed = store
        .claim_pending_commands("dispatcher-second", Utc::now() + Duration::minutes(1), 10)
        .await?
        .into_iter()
        .find(|record| record.id == command_id)
        .expect("expired dispatch lease should be reclaimed");
    assert_eq!(
        reclaimed.dispatch_claim_generation,
        first_claim.dispatch_claim_generation + 1
    );
    assert!(matches!(
        store
            .defer_claimed_command_if_owned(
                &command_id,
                "dispatcher-second",
                reclaimed.dispatch_claim_generation,
                test_dispatch_barrier("reclaimed command"),
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
async fn deferred_command_generation_overflow_rolls_back() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("implementing").with_id("deferred-generation-overflow");
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("implement_issue", "generation-overflow");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    sqlx::query(
        "UPDATE workflow_commands SET dispatch_claim_generation = $2 WHERE id = $1",
    )
    .bind(&command_id)
    .bind(i64::MAX)
    .execute(store.pool())
    .await?;

    let error = store
        .claim_pending_commands("overflow-owner", Utc::now() + Duration::minutes(1), 10)
        .await
        .expect_err("BIGINT generation overflow must fail the atomic claim");
    assert!(format!("{error:#}").contains("out of range"));
    let persisted = store.get_command(&command_id).await?.expect("command exists");
    assert_eq!(persisted.status, WorkflowCommandStatus::Pending);
    assert_eq!(persisted.dispatch_claim_generation, i64::MAX as u64);
    assert!(persisted.dispatch_owner.is_none());
    Ok(())
}

#[tokio::test]
async fn deferred_command_backoff_database() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (_, command_id, first_claim) =
        claimed_deferred_test_command(&store, "backoff", "backoff-1").await?;
    let policy = DispatchBackoffPolicy::from_seconds(2, 5)?;
    let first_now = Utc::now();
    let first = store
        .defer_claimed_command_if_owned(
            &command_id,
            "backoff-1",
            first_claim.dispatch_claim_generation,
            test_dispatch_barrier("backoff attempt one"),
            first_now,
            policy,
        )
        .await?;
    let DeferClaimedCommandOutcome::Deferred(first) = first else {
        panic!("first attempt should defer")
    };
    assert_eq!(first.next_dispatch_at, first_now + Duration::seconds(2));
    assert!(store
        .claim_pending_commands("too-early", Utc::now() + Duration::minutes(1), 10)
        .await?
        .is_empty());

    for (attempt, expected_delay) in [(2_u64, 4_i64), (3, 5), (4, 5)] {
        force_deferred_due(&store, &command_id).await?;
        let owner = format!("backoff-{attempt}");
        let claim = store
            .claim_pending_commands(&owner, Utc::now() + Duration::minutes(1), 10)
            .await?
            .into_iter()
            .find(|record| record.id == command_id)
            .expect("due command should be reclaimed");
        let now = Utc::now();
        let outcome = store
            .defer_claimed_command_if_owned(
                &command_id,
                &owner,
                claim.dispatch_claim_generation,
                test_dispatch_barrier("bounded retry"),
                now,
                policy,
            )
            .await?;
        let DeferClaimedCommandOutcome::Deferred(barrier) = outcome else {
            panic!("due attempt should defer")
        };
        assert_eq!(barrier.attempt, attempt);
        assert_eq!(barrier.next_dispatch_at, now + Duration::seconds(expected_delay));
    }
    Ok(())
}

#[tokio::test]
async fn deferred_command_survives_store_restart() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("workflow_runtime.db");
    let store = WorkflowRuntimeStore::open(&path).await?;
    let (workflow, command_id, claim) =
        claimed_deferred_test_command(&store, "restart", "restart-before").await?;
    let deferred = store
        .defer_claimed_command_if_owned(
            &command_id,
            "restart-before",
            claim.dispatch_claim_generation,
            test_dispatch_barrier("persist across restart"),
            Utc::now(),
            DispatchBackoffPolicy::from_seconds(2, 10)?,
        )
        .await?;
    let DeferClaimedCommandOutcome::Deferred(before) = deferred else {
        panic!("command should defer before restart")
    };
    let before_updated_at = store
        .get_command(&command_id)
        .await?
        .expect("command exists")
        .updated_at;
    drop(store);

    let store = WorkflowRuntimeStore::open(&path).await?;
    let after = store.get_command(&command_id).await?.expect("command survives");
    assert_eq!(after.status, WorkflowCommandStatus::Deferred);
    assert_eq!(after.dispatch_barrier.as_ref(), Some(&before));
    assert_eq!(after.updated_at, before_updated_at);
    assert_eq!(deferred_event_count(&store, &workflow.id).await?, 1);
    force_deferred_due(&store, &command_id).await?;
    let reclaimed = store
        .claim_pending_commands("restart-after", Utc::now() + Duration::minutes(1), 10)
        .await?
        .into_iter()
        .find(|record| record.id == command_id)
        .expect("due deferred command should be eligible after restart");
    assert_eq!(reclaimed.dispatch_claim_generation, claim.dispatch_claim_generation + 1);
    eprintln!(
        "MANUAL_DEFERRED_RESTART command_id={command_id} event_count=1 before_updated_at={before_updated_at} after_updated_at={} reclaimed_generation={}",
        after.updated_at, reclaimed.dispatch_claim_generation
    );
    Ok(())
}

#[tokio::test]
async fn deferred_command_dispatches_original_command_once() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (workflow, command_id, first_claim) =
        claimed_deferred_test_command(&store, "identity", "identity-before").await?;
    let original = store.get_command(&command_id).await?.expect("command exists");
    assert!(matches!(
        store
            .defer_claimed_command_if_owned(
                &command_id,
                "identity-before",
                first_claim.dispatch_claim_generation,
                test_dispatch_barrier("identity retry"),
                Utc::now(),
                DispatchBackoffPolicy::default(),
            )
            .await?,
        DeferClaimedCommandOutcome::Deferred(_)
    ));
    force_deferred_due(&store, &command_id).await?;
    let retry = store
        .claim_pending_commands("identity-after", Utc::now() + Duration::minutes(1), 10)
        .await?
        .into_iter()
        .find(|record| record.id == command_id)
        .expect("due command should be claimed");
    let dispatch_claim = DispatchClaim {
        owner: "identity-after",
        generation: retry.dispatch_claim_generation,
    };
    let input = json!({
        "workflow_id": workflow.id,
        "command_id": command_id,
        "dedupe_key": original.command.dedupe_key,
        "activity": "implement_issue"
    });
    let first = store
        .enqueue_runtime_job_for_claimed_command(
            &command_id,
            dispatch_claim,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            input.clone(),
            None,
        )
        .await?;
    let RuntimeJobEnqueueOutcome::Enqueued(job) = first else {
        panic!("repaired barrier should enqueue the original command")
    };
    let replay = store
        .enqueue_runtime_job_for_claimed_command(
            &command_id,
            dispatch_claim,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            input,
            None,
        )
        .await?;
    let RuntimeJobEnqueueOutcome::AlreadyExists(replayed_job) = replay else {
        panic!("same-generation replay should be write-free and idempotent")
    };
    assert_eq!(replayed_job.id, job.id);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].command_id, original.id);
    let final_command = store.get_command(&command_id).await?.expect("command exists");
    assert_eq!(final_command.status, WorkflowCommandStatus::Dispatched);
    assert_eq!(final_command.command.dedupe_key, original.command.dedupe_key);
    assert!(final_command.dispatch_not_before.is_none());
    assert!(final_command.dispatch_barrier.is_none());
    let event = store
        .events_for(&workflow.id)
        .await?
        .into_iter()
        .find(|event| event.event_type == "WorkflowRuntimeDispatchDeferred")
        .expect("deferral audit event exists");
    eprintln!(
        "MANUAL_DEFERRED_LIFECYCLE command_id={command_id} event_id={} job_id={} dedupe_key={}",
        event.id, job.id, final_command.command.dedupe_key
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_migrates_deferred_commands() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("workflow_runtime.db");
    let store = WorkflowRuntimeStore::open(&path).await?;
    let workflow = issue_instance("implementing").with_id("deferred-migration");
    store.upsert_instance(&workflow).await?;
    let legacy_statuses = [
        WorkflowCommandStatus::Pending,
        WorkflowCommandStatus::Dispatching,
        WorkflowCommandStatus::Dispatched,
        WorkflowCommandStatus::HandledInline,
        WorkflowCommandStatus::Completed,
        WorkflowCommandStatus::Failed,
        WorkflowCommandStatus::Blocked,
        WorkflowCommandStatus::Cancelled,
        WorkflowCommandStatus::Skipped,
    ];
    let mut command_ids = Vec::new();
    for status in legacy_statuses {
        let command = WorkflowCommand::enqueue_activity(
            "implement_issue",
            format!("legacy-status-{}", status.as_str()),
        );
        command_ids.push((
            store
                .enqueue_command_with_status(&workflow.id, None, &command, status)
                .await?,
            status,
        ));
    }
    sqlx::query("DELETE FROM schema_migrations WHERE version = 19")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP INDEX IF EXISTS idx_workflow_commands_dispatch_claim")
        .execute(store.pool())
        .await?;
    sqlx::query(
        "ALTER TABLE workflow_commands
         DROP CONSTRAINT IF EXISTS workflow_commands_dispatch_claim_generation_check,
         DROP CONSTRAINT IF EXISTS workflow_commands_status_check,
         DROP COLUMN dispatch_not_before,
         DROP COLUMN dispatch_attempt_count,
         DROP COLUMN dispatch_claim_generation,
         DROP COLUMN dispatch_barrier",
    )
    .execute(store.pool())
    .await?;
    sqlx::query(
        "ALTER TABLE workflow_commands ADD CONSTRAINT workflow_commands_status_check
         CHECK (status IN (
           'pending', 'dispatching', 'dispatched', 'handled_inline', 'completed',
           'failed', 'blocked', 'cancelled', 'skipped'
         ))",
    )
    .execute(store.pool())
    .await?;
    drop(store);

    let store = WorkflowRuntimeStore::open(&path).await?;
    let constraint_command_id = command_ids
        .first()
        .expect("legacy command fixture exists")
        .0
        .clone();
    for (command_id, expected_status) in command_ids {
        let migrated = store.get_command(&command_id).await?.expect("legacy row survives");
        assert_eq!(migrated.status, expected_status);
        assert_eq!(migrated.dispatch_attempt_count, 0);
        assert_eq!(migrated.dispatch_claim_generation, 0);
        assert!(migrated.dispatch_not_before.is_none());
        assert!(migrated.dispatch_barrier.is_none());
    }
    let constraint_error = sqlx::query(
        "UPDATE workflow_commands SET dispatch_claim_generation = -1 WHERE id = $1",
    )
    .bind(constraint_command_id)
    .execute(store.pool())
    .await
    .expect_err("migrated generation constraint rejects negative values");
    assert!(format!("{constraint_error:#}")
        .contains("workflow_commands_dispatch_claim_generation_check"));
    Ok(())
}
