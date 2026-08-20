use super::runtime_hosts_workflow_api_tests as support;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivitySignal, RuntimeJobStatus, RuntimeKind,
    WorkflowRuntimeStore, WorkflowSubject,
};
use serde_json::json;

type DatabaseActivityDiagnostic = (i32, String, Option<String>, Option<String>, Vec<i32>);

async fn open_race_store(store: &WorkflowRuntimeStore) -> anyhow::Result<WorkflowRuntimeStore> {
    let schema: String = sqlx::query_scalar("SELECT current_schema()")
        .fetch_one(store.pool())
        .await?;
    let database_url = std::env::var("HARNESS_DATABASE_URL")?;
    WorkflowRuntimeStore::open_with_database_url_and_schema(Some(&database_url), &schema).await
}

fn ordinary_eval_input() -> serde_json::Value {
    json!({
        "activity": "implement_issue",
        "eval": {
            "timeout_secs": 60,
            "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    })
}

fn ordinary_eval_completion_request(claimed: &serde_json::Value) -> serde_json::Value {
    json!({
        "lease_generation": claimed["lease_generation"],
        "lease_expires_at": claimed["lease_expires_at"],
        "lease_proof": claimed["lease_proof"],
        "result": ActivityResult::succeeded("implement_issue", "ordinary completion"),
        "execution_evidence": {
            "checked_out_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "resource_limit_report": {
                "limits": {
                    "requested": {
                        "cpu_time_secs": 60,
                        "memory_bytes": 8589934592_u64,
                        "pids": 512,
                        "disk_bytes": 21474836480_u64,
                        "output_bytes": 67108864,
                        "wall_time_secs": 60
                    },
                    "effective": {
                        "cpu_time_secs": 60,
                        "memory_bytes": 8589934592_u64,
                        "pids": 512,
                        "disk_bytes": 21474836480_u64,
                        "output_bytes": 67108864,
                        "wall_time_secs": 60
                    }
                },
                "usage": {
                    "cpu_time_millis": 100,
                    "peak_memory_bytes": 1024,
                    "peak_pids": 2,
                    "disk_bytes": 4096,
                    "output_bytes": 128,
                    "wall_time_millis": 1000
                },
                "reason": "completed within resource limits"
            },
            "usage": {
                "model": "test-model",
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15
            },
            "isolation_cleanup_status": "cleaned"
        }
    })
}

fn terminal_issue_result() -> ActivityResult {
    ActivityResult::succeeded("implement_issue", "issue closed during completion")
        .with_signal(ActivitySignal::new(
            "IssueClosed",
            json!({
                "issue_number": 123,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/123"
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            harness_workflow::runtime::completion_evidence::ARTIFACT_VERIFIED_ISSUE_STATE,
            json!({
                "issue_number": 123,
                "repo": "owner/repo",
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/123",
                "snapshot_source": "server_github_rest"
            }),
        ))
}

fn assert_cleanup_ack_required(status: axum::http::StatusCode, body: &serde_json::Value) {
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error_code"], "lease_lost");
    assert_eq!(body["must_stop"], true);
    assert_eq!(body["cleanup_ack_required"], true);
}

async fn wait_for_gate_blockers(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    gate_backend_pid: i32,
    expected: i64,
) -> anyhow::Result<()> {
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let blockers: i64 = sqlx::query_scalar(
                "WITH RECURSIVE blocked(pid) AS (
                     SELECT activity.pid
                     FROM pg_stat_activity AS activity
                     WHERE activity.datname = current_database()
                       AND $1 = ANY(pg_blocking_pids(activity.pid))
                     UNION
                     SELECT activity.pid
                     FROM pg_stat_activity AS activity
                     JOIN blocked AS predecessor
                       ON predecessor.pid = ANY(pg_blocking_pids(activity.pid))
                     WHERE activity.datname = current_database()
                 )
                 SELECT COUNT(*) FROM blocked",
            )
            .bind(gate_backend_pid)
            .fetch_one(store.pool())
            .await?;
            if blockers >= expected {
                return anyhow::Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if waited.is_err() {
        let activity: Vec<DatabaseActivityDiagnostic> = sqlx::query_as(
            "SELECT pid, state, wait_event_type, wait_event, pg_blocking_pids(pid)
                 FROM pg_stat_activity
                 WHERE datname = current_database()
                 ORDER BY pid",
        )
        .fetch_all(store.pool())
        .await?;
        anyhow::bail!(
            "timed out waiting for {expected} blocked backends behind {gate_backend_pid}: \
             {activity:?}"
        );
    }
    waited.expect("timeout handled")?;
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_preserves_cancelled_eval_feedback_cleanup_proof(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "cancelled-eval-feedback",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "command": {
                "activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "eval": {
                    "eval_run_id": "run-cancelled-feedback",
                    "case_id": "case-1",
                    "timeout_secs": 45
                }
            }
        }),
    )
    .await?;
    let claimed = support::post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    store
        .cancel_command_and_unfinished_runtime_jobs(
            &job.command_id,
            harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "operator cancelled eval",
        )
        .await?;

    let completed = support::post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "result": ActivityResult::cancelled(
                harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "host stopped and cleaned",
            ),
            "execution_evidence": {
                "checked_out_commit": "",
                "resource_limit_report": {},
                "usage": {
                    "model": "test-model",
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cached_input_tokens": 0,
                    "total_tokens": 0,
                    "cost_usd_micros": 0
                },
                "isolation_cleanup_status": "cleaned",
                "validation": []
            }
        }),
    )
    .await?;

    assert_eq!(completed["completed"], true);
    assert_eq!(completed["runtime_job"]["status"], "cancelled");
    assert!(completed["runtime_job"]["output"]["artifacts"]
        .as_array()
        .is_some_and(|artifacts| artifacts.iter().any(|artifact| {
            artifact["artifact_type"]
                == harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP
                && artifact["artifact"]["status"] == "cleaned"
                && artifact["artifact"]["evidence_source"] == "runtime_host_cancellation_ack"
        })));
    Ok(())
}

#[tokio::test]
async fn completion_reservation_reports_cleanup_ack_when_terminal_fence_wins_race(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;
    let job = support::enqueue_runtime_host_test_job(
        &store,
        "completion-terminal-fence-race",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        ordinary_eval_input(),
    )
    .await?;
    let claimed = support::post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    let cancellation_store = std::sync::Arc::new(open_race_store(&store).await?);
    let gate_store = open_race_store(&store).await?;
    let monitor_store = open_race_store(&store).await?;

    let mut gate = gate_store.pool().begin().await?;
    let gate_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await?;
    sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE")
        .bind(&job.id)
        .execute(&mut *gate)
        .await?;

    let command_id = job.command_id.clone();
    let cancellation = tokio::spawn(async move {
        cancellation_store
            .cancel_command_and_unfinished_runtime_jobs(
                &command_id,
                "implement_issue",
                "terminal fence won completion race",
            )
            .await
    });
    wait_for_gate_blockers(&monitor_store, gate_backend_pid, 1).await?;

    let runtime_job_id = job.id.clone();
    let completion = tokio::spawn(async move {
        support::post_json_with_status(
            &app,
            format!("/api/runtime-hosts/host-a/runtime-jobs/{runtime_job_id}/complete"),
            ordinary_eval_completion_request(&claimed),
        )
        .await
    });
    if let Err(error) = wait_for_gate_blockers(&monitor_store, gate_backend_pid, 2).await {
        if completion.is_finished() {
            let (status, body) = completion.await??;
            anyhow::bail!("completion returned before reservation race: {status} {body}");
        }
        return Err(error);
    }
    gate.rollback().await?;

    assert_eq!(cancellation.await??, 1);
    let (status, body) = completion.await??;
    assert_cleanup_ack_required(status, &body);
    Ok(())
}

#[tokio::test]
async fn completion_commit_reports_cleanup_ack_when_post_reservation_fence_wins_race(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;
    let key = "post-reservation-terminal-fence-race";
    let workflow_id = format!("runtime-host-test-{key}");
    let job = support::enqueue_runtime_host_test_job(
        &store,
        key,
        RuntimeKind::RemoteHost,
        "remote-host-default",
        ordinary_eval_input(),
    )
    .await?;
    let mut workflow = store
        .get_instance(&workflow_id)
        .await?
        .expect("race workflow should exist");
    workflow.subject = WorkflowSubject::new("issue", "123");
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &workflow).await?;
    let claimed = support::post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    let claimed_expires_at: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(claimed["lease_expires_at"].clone())?;

    let terminal_store = std::sync::Arc::new(open_race_store(&store).await?);
    let gate_store = open_race_store(&store).await?;
    let monitor_store = open_race_store(&store).await?;
    let mut gate = gate_store.pool().begin().await?;
    let gate_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await?;
    sqlx::query("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE")
        .bind(&workflow_id)
        .execute(&mut *gate)
        .await?;

    let terminal_workflow_id = workflow_id.clone();
    let terminal = tokio::spawn(async move {
        terminal_store
            .commit_parent_runtime_completion(
                &terminal_workflow_id,
                "post-reservation-terminal-fence",
                json!({
                    "command_id": "post-reservation-terminal-command",
                    "runtime_job_id": "post-reservation-terminal-job",
                    "activity_result": terminal_issue_result(),
                }),
            )
            .await
    });
    wait_for_gate_blockers(&monitor_store, gate_backend_pid, 1).await?;

    let runtime_job_id = job.id.clone();
    let completion = tokio::spawn(async move {
        support::post_json_with_status(
            &app,
            format!("/api/runtime-hosts/host-a/runtime-jobs/{runtime_job_id}/complete"),
            ordinary_eval_completion_request(&claimed),
        )
        .await
    });
    if let Err(error) = wait_for_gate_blockers(&monitor_store, gate_backend_pid, 2).await {
        if completion.is_finished() {
            let (status, body) = completion.await??;
            anyhow::bail!("completion returned before commit race: {status} {body}");
        }
        return Err(error);
    }
    let reserved = monitor_store
        .get_runtime_job(&job.id)
        .await?
        .expect("reserved runtime job should remain readable");
    assert!(
        reserved
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > claimed_expires_at),
        "completion lease must be reserved before the terminal fence commits"
    );
    gate.rollback().await?;

    let terminal = terminal
        .await??
        .expect("closed issue should produce a terminal decision");
    assert!(terminal.accepted);
    assert_eq!(terminal.decision.next_state, "done");
    let (status, body) = completion.await??;
    assert_cleanup_ack_required(status, &body);
    let cancelling = store
        .get_runtime_job(&job.id)
        .await?
        .expect("terminal-fenced eval should remain readable");
    assert_eq!(cancelling.status, RuntimeJobStatus::Running);
    assert!(cancelling.input.get("cancellation_requested").is_some());
    Ok(())
}

#[tokio::test]
async fn stale_dead_letter_reports_cleanup_ack_when_terminal_fence_wins_race() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = support::make_test_state_with_runtime_store(dir.path()).await?
    else {
        return Ok(());
    };
    let app = support::runtime_hosts_workflow_app(state);
    support::register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;
    let key = "stale-dead-letter-terminal-fence-race";
    let workflow_id = format!("runtime-host-test-{key}");
    let job = support::enqueue_runtime_host_test_job(
        &store,
        key,
        RuntimeKind::RemoteHost,
        "remote-host-default",
        ordinary_eval_input(),
    )
    .await?;
    let mut workflow = store
        .get_instance(&workflow_id)
        .await?
        .expect("race workflow should exist");
    workflow.subject = WorkflowSubject::new("issue", "123");
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &workflow).await?;
    let expired_at = chrono::Utc::now() - chrono::TimeDelta::seconds(1);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(RuntimeKind::RemoteHost, "host-a", expired_at)
        .await?
        .expect("remote eval should be claimed with an expired lease");
    let lease_proof = store
        .remote_runtime_job_lease_proof(&job.id, "host-a", claimed.lease_generation, expired_at)
        .await?
        .expect("expired lease should retain its issuance proof");
    let completion_request = ordinary_eval_completion_request(&json!({
        "lease_generation": claimed.lease_generation,
        "lease_expires_at": expired_at,
        "lease_proof": lease_proof,
    }));

    let terminal_store = std::sync::Arc::new(open_race_store(&store).await?);
    let gate_store = open_race_store(&store).await?;
    let monitor_store = open_race_store(&store).await?;
    let mut gate = gate_store.pool().begin().await?;
    let gate_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await?;
    sqlx::query("SELECT id FROM workflow_instances WHERE id = $1 FOR UPDATE")
        .bind(&workflow_id)
        .execute(&mut *gate)
        .await?;

    let terminal_workflow_id = workflow_id.clone();
    let terminal = tokio::spawn(async move {
        terminal_store
            .commit_parent_runtime_completion(
                &terminal_workflow_id,
                "stale-dead-letter-terminal-fence",
                json!({
                    "command_id": "stale-dead-letter-terminal-command",
                    "runtime_job_id": "stale-dead-letter-terminal-job",
                    "activity_result": terminal_issue_result(),
                }),
            )
            .await
    });
    wait_for_gate_blockers(&monitor_store, gate_backend_pid, 1).await?;

    let runtime_job_id = job.id.clone();
    let completion = tokio::spawn(async move {
        support::post_json_with_status(
            &app,
            format!("/api/runtime-hosts/host-a/runtime-jobs/{runtime_job_id}/complete"),
            completion_request,
        )
        .await
    });
    if let Err(error) = wait_for_gate_blockers(&monitor_store, gate_backend_pid, 2).await {
        if completion.is_finished() {
            let (status, body) = completion.await??;
            anyhow::bail!("completion returned before stale DLQ race: {status} {body}");
        }
        return Err(error);
    }
    let dead_letters: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(monitor_store.pool())
    .await?;
    assert_eq!(
        dead_letters, 0,
        "stale completion must still be awaiting DLQ classification"
    );
    gate.rollback().await?;

    let terminal = terminal
        .await??
        .expect("closed issue should produce a terminal decision");
    assert!(terminal.accepted);
    assert_eq!(terminal.decision.next_state, "done");
    let (status, body) = completion.await??;
    assert_cleanup_ack_required(status, &body);
    let dead_letters: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        dead_letters, 0,
        "cancelled eval completion must not be dead-lettered"
    );
    Ok(())
}
