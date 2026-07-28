//! Concurrent dispatch + completion on a single workflow.
//!
//! Both families take row locks that touch the same `workflow_instances` row:
//! dispatch locks the instance then the command, completion locks the command,
//! the runtime job, and (through the completion reducer) the instance. Before
//! the ordering fix these interleaved into an ABBA cycle, which PostgreSQL
//! breaks by aborting one side with SQLSTATE 40P01 — surfacing as a failed
//! runtime job rather than a retried transaction.

use super::*;
use std::sync::Arc;
use tokio::sync::Barrier;

/// Commands per side. Enough interleaving to expose a lock cycle without
/// making the test slow.
const PAIRS: usize = 8;

fn is_deadlock_error(error: &anyhow::Error) -> bool {
    let rendered = format!("{error:#}").to_ascii_lowercase();
    rendered.contains("40p01") || rendered.contains("deadlock")
}

async fn assert_at_least_two_database_connections(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<()> {
    let mut first = store.pool().acquire().await?;
    let mut second =
        tokio::time::timeout(std::time::Duration::from_secs(2), store.pool().acquire())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "lock-order stress requires at least two simultaneous PostgreSQL connections"
                )
            })??;
    let (first_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *first)
        .await?;
    let (second_backend,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
        .fetch_one(&mut *second)
        .await?;
    assert_ne!(
        first_backend, second_backend,
        "lock-order stress must run against two distinct PostgreSQL backends"
    );
    Ok(())
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
    anyhow::bail!("timed out waiting for a second PostgreSQL backend to block");
}

#[tokio::test]
async fn event_writer_locks_instance_before_sequence_advisory() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("event-lock-order.db")).await?);
    assert_at_least_two_database_connections(&store).await?;

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
async fn concurrent_dispatch_and_completion_on_one_workflow_never_deadlocks() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?);
    assert_at_least_two_database_connections(&store).await?;

    // One workflow instance: every transaction below contends for this row.
    let workflow = issue_instance("implementing").with_id("lock-order-stress");
    store.upsert_instance(&workflow).await?;

    // Both sides work the SAME command rows. That is what closes the cycle:
    // the dispatcher holds the instance and waits for the command, while the
    // completion holds that command and waits for the instance. Splitting the
    // commands between the sides would leave the two transactions waiting on
    // rows the other never wants, and no deadlock could form.
    let mut commands = Vec::new();
    for index in 0..PAIRS {
        let command = WorkflowCommand::enqueue_activity(
            "implement_issue",
            format!("lock-order-stress-{index}"),
        );
        commands.push(store.enqueue_command(&workflow.id, None, &command).await?);
    }

    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_pending_commands("stress-dispatcher", lease_expires_at, PAIRS as i64)
        .await?;
    let generation_of = |command_id: &str| -> anyhow::Result<u64> {
        claimed
            .iter()
            .find(|record| record.id == command_id)
            .map(|record| record.dispatch_claim_generation)
            .ok_or_else(|| anyhow::anyhow!("command {command_id} should have been claimed"))
    };

    // Give the completion side real running jobs to finish.
    let mut running_jobs = Vec::new();
    for command_id in &commands {
        store
            .enqueue_runtime_job_for_claimed_command(
                command_id,
                DispatchClaim {
                    owner: "stress-dispatcher",
                    generation: generation_of(command_id)?,
                },
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({ "activity": "implement_issue" }),
                None,
            )
            .await?;
        let job = store
            .claim_next_runtime_job("stress-worker", lease_expires_at)
            .await?
            .ok_or_else(|| anyhow::anyhow!("enqueued runtime job should be claimable"))?;
        running_jobs.push(job);
    }

    // Release both sides at once so the transactions actually interleave.
    let barrier = Arc::new(Barrier::new(PAIRS * 2));
    let mut handles = Vec::new();

    for command_id in commands {
        let store = store.clone();
        let barrier = barrier.clone();
        let generation = generation_of(&command_id)?;
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .enqueue_runtime_job_for_claimed_command(
                    &command_id,
                    DispatchClaim {
                        owner: "stress-dispatcher",
                        generation,
                    },
                    RuntimeKind::CodexJsonrpc,
                    "codex-default",
                    json!({ "activity": "implement_issue" }),
                    None,
                )
                .await
                .map(|_| ())
        }));
    }

    for job in running_jobs {
        let store = store.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .commit_runtime_activity_completion_if_owned(
                    &job.id,
                    "stress-worker",
                    lease_expires_at,
                    &ActivityResult::succeeded("implement_issue", "stress completion"),
                )
                .await
                .map(|_| ())
        }));
    }

    let mut deadlocks = Vec::new();
    let mut failures = Vec::new();
    for handle in handles {
        match handle.await? {
            Ok(()) => {}
            Err(error) if is_deadlock_error(&error) => deadlocks.push(format!("{error:#}")),
            Err(error) => failures.push(format!("{error:#}")),
        }
    }

    assert!(
        deadlocks.is_empty(),
        "a deadlock abort reached the caller instead of being ordered away or retried:\n{}",
        deadlocks.join("\n")
    );
    assert!(
        failures.is_empty(),
        "concurrent dispatch/completion failed:\n{}",
        failures.join("\n")
    );
    Ok(())
}
