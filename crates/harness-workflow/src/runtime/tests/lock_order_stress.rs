//! Deterministic PostgreSQL lock-order regressions for workflow runtime actors.
//!
//! Each test holds a transaction gate until the production actor reaches a
//! specific lock, observes the actual backend PID through `pg_blocking_pids`,
//! and then starts the competing actor. The gates turn historical ABBA races
//! into reproducible schedules instead of probabilistic stress.

use super::*;
use crate::runtime::{
    WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome, WorkflowRuntimeRecoveryRequest,
};
use std::sync::Arc;

type BackendWait = (
    i32,
    String,
    Option<String>,
    Option<String>,
    Vec<i32>,
    String,
);

fn is_deadlock_error(error: &anyhow::Error) -> bool {
    let rendered = format!("{error:#}").to_ascii_lowercase();
    rendered.contains("40p01") || rendered.contains("deadlock")
}

async fn wait_for_backend_blocked_by(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    blocker_backend: i32,
) -> anyhow::Result<i32> {
    for _ in 0..100 {
        let blocked: Option<(i32,)> = sqlx::query_as(
            "SELECT pid
             FROM pg_stat_activity
             WHERE pid <> $1
               AND $1 = ANY(pg_blocking_pids(pid))
             LIMIT 1",
        )
        .bind(blocker_backend)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((blocked_backend,)) = blocked {
            return Ok(blocked_backend);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let waits: Vec<BackendWait> = sqlx::query_as(
        "SELECT pid, state, wait_event_type, wait_event, pg_blocking_pids(pid), query
         FROM pg_stat_activity
         WHERE datname = current_database() AND state <> 'idle'",
    )
    .fetch_all(&mut **tx)
    .await?;
    anyhow::bail!(
        "timed out waiting for a second PostgreSQL backend to block; observed waits: {waits:?}"
    );
}

#[tokio::test]
#[ignore = "requires an isolated HARNESS_DATABASE_URL"]
async fn event_writer_locks_instance_before_sequence_advisory() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("event-lock-order.db")).await?);

    let workflow = issue_instance("implementing").with_id("event-lock-order");
    store.upsert_instance(&workflow).await?;

    let mut blocker = store.pool().begin().await?;
    let (blocker_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;
    sqlx::query("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE")
        .bind(&workflow.id)
        .execute(&mut *blocker)
        .await?;

    let writer_store = store.clone();
    let workflow_id = workflow.id.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .append_event(
                &workflow_id,
                "LockOrderProbe",
                "lock_order_stress",
                json!({}),
            )
            .await
    });

    let blocked_backend = wait_for_backend_blocked_by(&mut blocker, blocker_backend).await?;
    assert_ne!(
        blocker_backend, blocked_backend,
        "event writer must run on a second PostgreSQL backend"
    );
    let advisory_key = format!("workflow_events:{}", workflow.id);
    let (advisory_available,): (bool,) =
        sqlx::query_as("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&advisory_key)
            .fetch_one(&mut *blocker)
            .await?;

    blocker.rollback().await?;
    let joined = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
        .await
        .map_err(|_| anyhow::anyhow!("event writer did not finish after releasing the row lock"))?;
    let event = joined??;

    assert!(
        advisory_available,
        "event writer acquired the sequence advisory before its instance parent lock"
    );
    assert_eq!(event.workflow_id, workflow.id);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated HARNESS_DATABASE_URL"]
async fn runtime_event_writer_locks_job_before_sequence_advisory() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(
        WorkflowRuntimeStore::open(&dir.path().join("runtime-event-lock-order.db")).await?,
    );
    let job = enqueue_test_runtime_job(
        &store,
        "runtime-event-lock-order",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "implement_issue"}),
    )
    .await?;

    let mut blocker = store.pool().begin().await?;
    let (blocker_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await?;
    sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE")
        .bind(&job.id)
        .execute(&mut *blocker)
        .await?;

    let writer_store = Arc::clone(&store);
    let runtime_job_id = job.id.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .record_runtime_event(
                &runtime_job_id,
                "RuntimeLockOrderProbe",
                json!({"source": "lock_order_stress"}),
            )
            .await
    });

    let writer_backend = wait_for_backend_blocked_by(&mut blocker, blocker_backend).await?;
    assert_ne!(
        blocker_backend, writer_backend,
        "runtime event writer must execute on a distinct PostgreSQL backend"
    );

    sqlx::query("SET LOCAL deadlock_timeout = '100ms'")
        .execute(&mut *blocker)
        .await?;
    let advisory_key = format!("runtime_events:{}", job.id);
    let advisory_result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&advisory_key)
            .execute(&mut *blocker),
    )
    .await;

    let blocker_rollback = blocker.rollback().await;
    let writer_result = tokio::time::timeout(std::time::Duration::from_secs(5), writer)
        .await
        .map_err(|_| {
            anyhow::anyhow!("runtime event writer did not finish after releasing the job lock")
        })?;
    blocker_rollback?;
    match advisory_result {
        Err(_) => anyhow::bail!(
            "runtime event advisory lock remained blocked behind its own parent-row dependency"
        ),
        Ok(Err(error)) => {
            return Err(anyhow::anyhow!(
                "runtime event lock order produced a PostgreSQL deadlock: {error}"
            ));
        }
        Ok(Ok(_)) => {}
    }
    let event = writer_result??;

    assert_eq!(event.runtime_job_id, job.id);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated HARNESS_DATABASE_URL"]
async fn dispatch_and_completion_form_an_observable_parent_first_wait_chain() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let database_path = dir.path().join("workflow-runtime-order.db");
    let store = Arc::new(WorkflowRuntimeStore::open(&database_path).await?);
    let completion_store = Arc::new(WorkflowRuntimeStore::open(&database_path).await?);
    let dispatch_store = Arc::new(WorkflowRuntimeStore::open(&database_path).await?);
    let mut dispatch_connection = dispatch_store.pool().acquire().await?;
    sqlx::query("SELECT 1")
        .execute(&mut *dispatch_connection)
        .await?;
    drop(dispatch_connection);
    let workflow = issue_instance("implementing").with_id("lock-order-observable-chain");
    store.upsert_instance(&workflow).await?;
    let command =
        WorkflowCommand::enqueue_activity("implement_issue", "lock-order-observable-command");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_pending_commands("stress-dispatcher", lease_expires_at, 1)
        .await?;
    let generation = claimed
        .first()
        .filter(|record| record.id == command_id)
        .map(|record| record.dispatch_claim_generation)
        .ok_or_else(|| anyhow::anyhow!("command should have been claimed"))?;
    let enqueue = store
        .enqueue_runtime_job_for_claimed_command(
            &command_id,
            DispatchClaim {
                owner: "stress-dispatcher",
                generation,
            },
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "implement_issue"}),
            None,
        )
        .await?;
    let RuntimeJobEnqueueOutcome::Enqueued(enqueued_job) = enqueue else {
        anyhow::bail!("expected a newly enqueued runtime job, got {enqueue:?}");
    };
    let running_job = store
        .claim_next_runtime_job("stress-worker", lease_expires_at)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should have been claimable"))?;
    assert_eq!(running_job.id, enqueued_job.id);

    sqlx::query(
        "CREATE OR REPLACE FUNCTION harness_test_completion_order_gate()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             PERFORM pg_advisory_xact_lock(
                 hashtextextended('runtime_job_completion_order_gate:' || OLD.id, 0)
             );
             RETURN NEW;
         END;
         $$",
    )
    .execute(store.pool())
    .await?;
    sqlx::query(
        "CREATE TRIGGER harness_test_completion_order_gate
         BEFORE UPDATE ON runtime_jobs
         FOR EACH ROW
         EXECUTE FUNCTION harness_test_completion_order_gate()",
    )
    .execute(store.pool())
    .await?;

    let gate_key = format!("runtime_job_completion_order_gate:{}", running_job.id);
    let mut gate = store.pool().begin().await?;
    let (gate_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&gate_key)
        .execute(&mut *gate)
        .await?;

    let completion_store = Arc::clone(&completion_store);
    let completion_job_id = running_job.id.clone();
    let mut completion = tokio::spawn(async move {
        completion_store
            .commit_runtime_activity_completion_if_owned(
                &completion_job_id,
                "stress-worker",
                lease_expires_at,
                &ActivityResult::succeeded("implement_issue", "stress completion"),
            )
            .await
    });
    let completion_backend = tokio::select! {
        blocked = wait_for_backend_blocked_by(&mut gate, gate_backend) => {
            blocked.map_err(|error| anyhow::anyhow!("completion never reached the runtime-job gate: {error}"))?
        },
        result = &mut completion => {
            anyhow::bail!("completion finished before reaching the runtime-job gate: {result:?}");
        }
    };

    let dispatch_store = Arc::clone(&dispatch_store);
    let dispatch_command_id = command_id.clone();
    let mut dispatch = tokio::spawn(async move {
        dispatch_store
            .enqueue_runtime_job_for_claimed_command(
                &dispatch_command_id,
                DispatchClaim {
                    owner: "stress-dispatcher",
                    generation,
                },
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({"activity": "implement_issue"}),
                None,
            )
            .await
    });
    let dispatch_backend = tokio::select! {
        blocked = wait_for_backend_blocked_by(&mut gate, completion_backend) => {
            blocked.map_err(|error| anyhow::anyhow!("dispatch never waited on completion: {error}"))?
        },
        result = &mut completion => {
            anyhow::bail!("completion exited while the runtime-job gate was still held: {result:?}");
        },
        result = &mut dispatch => {
            anyhow::bail!("dispatch finished before reaching the instance lock: {result:?}");
        }
    };
    assert_ne!(gate_backend, completion_backend);
    assert_ne!(gate_backend, dispatch_backend);
    assert_ne!(
        completion_backend, dispatch_backend,
        "completion and dispatch must be observed on distinct PostgreSQL backends"
    );

    gate.rollback().await?;
    let completion = tokio::time::timeout(std::time::Duration::from_secs(5), completion)
        .await
        .map_err(|_| {
            anyhow::anyhow!("completion did not finish after releasing the test gate")
        })???;
    let dispatch = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
        .await
        .map_err(|_| anyhow::anyhow!("dispatch did not finish after completion committed"))???;

    sqlx::query("DROP TRIGGER harness_test_completion_order_gate ON runtime_jobs")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP FUNCTION harness_test_completion_order_gate()")
        .execute(store.pool())
        .await?;

    assert!(
        completion.is_some(),
        "the observed completion actor must own and commit its runtime job"
    );
    assert_eq!(dispatch, RuntimeJobEnqueueOutcome::StaleClaim);
    Ok(())
}

#[tokio::test]
#[ignore = "requires an isolated HARNESS_DATABASE_URL"]
async fn recovery_and_lease_revoke_lock_runtime_jobs_in_global_id_order() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let database_path = dir.path().join("recovery-job-order.db");
    let store = Arc::new(WorkflowRuntimeStore::open(&database_path).await?);
    let revoke_store = Arc::new(WorkflowRuntimeStore::open(&database_path).await?);
    let workflow = project_issue_instance("/lock-order-project", 1830, "blocked")
        .with_id("recovery-job-order");
    store.upsert_instance(&workflow).await?;

    let mut jobs_by_command = std::collections::BTreeMap::new();
    for suffix in ["first", "second"] {
        let command = WorkflowCommand::enqueue_activity(
            "implement_issue",
            format!("recovery-job-order-{suffix}"),
        );
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        let outcome = store
            .enqueue_runtime_job_for_pending_command(
                &command_id,
                RuntimeKind::RemoteHost,
                "remote-default",
                json!({"activity": "implement_issue"}),
                None,
            )
            .await?;
        let RuntimeJobEnqueueOutcome::Enqueued(job) = outcome else {
            anyhow::bail!("expected a newly enqueued runtime job, got {outcome:?}");
        };
        jobs_by_command.insert(command_id, job.id);
    }

    let command_order: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, status, data::text FROM workflow_commands
         WHERE workflow_id = $1
           AND status IN ('pending', 'dispatching', 'dispatched', 'deferred')",
    )
    .bind(&workflow.id)
    .fetch_all(store.pool())
    .await?;
    assert_eq!(command_order.len(), 2);
    let test_run_id = uuid::Uuid::new_v4();
    let controlled_job_ids = [
        format!("zz-recovery-job-order-{test_run_id}"),
        format!("aa-recovery-job-order-{test_run_id}"),
    ];
    for ((command_id, _, _), controlled_job_id) in
        command_order.iter().zip(controlled_job_ids.iter())
    {
        let original_job_id = jobs_by_command
            .get(command_id)
            .ok_or_else(|| anyhow::anyhow!("active command should own a runtime job"))?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET id = $1, data = jsonb_set(data, '{id}', to_jsonb($1::text), false)
             WHERE id = $2",
        )
        .bind(controlled_job_id)
        .bind(original_job_id)
        .execute(store.pool())
        .await?;
    }

    let owner = "lock-order-remote-host";
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    for _ in 0..2 {
        store
            .claim_next_runtime_job_for_runtime_kind(
                RuntimeKind::RemoteHost,
                owner,
                lease_expires_at,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("remote-host runtime job should be claimable"))?;
    }
    assert_eq!(
        store.count_remote_host_runtime_job_leases(owner).await?,
        2,
        "both jobs must be visible to the concurrent lease revocation"
    );

    let stopped = workflow.with_data(json!({
        "last_stop": {
            "state": "blocked",
            "activity": "implement_issue",
            "runtime_job_id": controlled_job_ids[0]
        }
    }));
    store.upsert_instance(&stopped).await?;
    let gate_key = format!("runtime_job_recovery_order_gate:{}", controlled_job_ids[0]);

    sqlx::query(
        "CREATE OR REPLACE FUNCTION harness_test_runtime_job_order_gate()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             IF OLD.id LIKE 'zz-recovery-job-order-%' THEN
                 PERFORM pg_advisory_xact_lock(
                     hashtextextended('runtime_job_recovery_order_gate:' || OLD.id, 0)
                 );
             END IF;
             RETURN NEW;
         END;
         $$",
    )
    .execute(store.pool())
    .await?;
    sqlx::query(
        "CREATE TRIGGER harness_test_runtime_job_order_gate
         BEFORE UPDATE ON runtime_jobs
         FOR EACH ROW
         EXECUTE FUNCTION harness_test_runtime_job_order_gate()",
    )
    .execute(store.pool())
    .await?;

    let mut gate = store.pool().begin().await?;
    let (gate_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&gate_key)
        .execute(&mut *gate)
        .await?;

    let recovery_store = Arc::clone(&store);
    let recovery_workflow_id = stopped.id.clone();
    let mut recovery = tokio::spawn(async move {
        recovery_store
            .recover_stopped_instance(WorkflowRuntimeRecoveryRequest {
                workflow_id: &recovery_workflow_id,
                action: WorkflowRuntimeRecoveryAction::Unblock,
                reason: "operator repaired the dependency",
                actor: "operator",
                target_state: None,
                evidence: &[],
            })
            .await
    });
    let recovery_backend = tokio::select! {
        blocked = wait_for_backend_blocked_by(&mut gate, gate_backend) => {
            blocked.map_err(|error| anyhow::anyhow!("recovery never reached the runtime-job gate: {error}"))?
        },
        result = &mut recovery => {
            anyhow::bail!("recovery finished before reaching the runtime-job gate: {result:?}");
        }
    };
    let revoke_store = Arc::clone(&revoke_store);
    let mut revoke = tokio::spawn(async move {
        revoke_store
            .revoke_remote_host_runtime_job_leases(owner, Utc::now())
            .await
    });
    let revoke_backend = tokio::select! {
        blocked = wait_for_backend_blocked_by(&mut gate, recovery_backend) => {
            blocked.map_err(|error| anyhow::anyhow!(
                "lease revoke never waited on recovery (pool size {}, idle {}): {error}",
                store.pool().size(),
                store.pool().num_idle(),
            ))?
        },
        result = &mut revoke => {
            anyhow::bail!("lease revoke finished before waiting on recovery: {result:?}");
        }
    };
    assert_ne!(gate_backend, recovery_backend);
    assert_ne!(gate_backend, revoke_backend);
    assert_ne!(
        recovery_backend, revoke_backend,
        "recovery and lease revoke must execute on distinct PostgreSQL backends"
    );

    gate.rollback().await?;
    let recovery_result = tokio::time::timeout(std::time::Duration::from_secs(10), recovery)
        .await
        .map_err(|_| anyhow::anyhow!("recovery did not finish after releasing the test gate"))?;
    let revoke_result = tokio::time::timeout(std::time::Duration::from_secs(10), revoke)
        .await
        .map_err(|_| anyhow::anyhow!("lease revoke did not finish after recovery"))?;

    sqlx::query("DROP TRIGGER harness_test_runtime_job_order_gate ON runtime_jobs")
        .execute(store.pool())
        .await?;
    sqlx::query("DROP FUNCTION harness_test_runtime_job_order_gate()")
        .execute(store.pool())
        .await?;

    let recovery_result = recovery_result?;
    let revoke_result = revoke_result?;
    if let Err(error) = &recovery_result {
        if is_deadlock_error(error) {
            anyhow::bail!("recovery reached PostgreSQL deadlock 40P01: {error:#}");
        }
    }
    if let Err(error) = &revoke_result {
        if is_deadlock_error(error) {
            anyhow::bail!("lease revoke reached PostgreSQL deadlock 40P01: {error:#}");
        }
    }
    let outcome = recovery_result?;
    let revoked = revoke_result?;
    assert!(matches!(
        outcome,
        WorkflowRuntimeRecoveryOutcome::Recovered { .. }
    ));
    assert_eq!(
        revoked, 0,
        "ordered recovery must finish cancellation before lease revoke rechecks rows"
    );
    Ok(())
}
