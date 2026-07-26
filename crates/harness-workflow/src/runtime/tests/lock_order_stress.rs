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

#[tokio::test]
async fn concurrent_dispatch_and_completion_on_one_workflow_never_deadlocks() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store =
        Arc::new(WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?);

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
