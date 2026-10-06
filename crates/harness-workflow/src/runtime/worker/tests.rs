use super::*;
use crate::runtime::{
    RuntimeJobStatus, RuntimeKind, WorkflowCommand, WorkflowCommandType, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject,
};
use async_trait::async_trait;
use harness_core::db::resolve_database_url;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct PreflightRuntimeExecutor {
    result: ActivityResult,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl RuntimeJobExecutor for PreflightRuntimeExecutor {
    async fn preflight_result(&self, _job: &RuntimeJob) -> Option<ActivityResult> {
        Some(self.result.clone())
    }

    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ActivityResult::succeeded("check", "executed")
    }
}

struct DeferringClaimGuard {
    not_before: DateTime<Utc>,
}

impl RuntimeJobClaimGuard for DeferringClaimGuard {
    fn before_execute(
        &self,
        _job: &RuntimeJob,
        _now: DateTime<Utc>,
        _lease_expires_at: DateTime<Utc>,
    ) -> RuntimeJobClaimDecision {
        RuntimeJobClaimDecision::Defer {
            not_before: self.not_before,
            reason: "test guard".to_string(),
        }
    }
}

async fn enqueue_test_runtime_job(
    store: &WorkflowRuntimeStore,
    key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
) -> anyhow::Result<RuntimeJob> {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", format!("issue:{key}")),
    )
    .with_id(format!("runtime-worker-test-{key}"));
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let activity = input
        .get("activity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("test_activity");
    let command = WorkflowCommand::enqueue_activity(activity, format!("runtime-worker-test-{key}"));
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
        .enqueue_runtime_job(&command_id, runtime_kind, runtime_profile, input)
        .await
}

#[test]
fn child_completion_payload_carries_parent_recovery_identity() {
    let child = WorkflowInstance::new(
        super::super::PR_FEEDBACK_DEFINITION_ID,
        1,
        "feedback_found",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child")
    .with_server_data(json!({
        "started_by_runtime_job_id": "parent-start-child-job",
    }));
    let event = super::super::model::WorkflowEvent::new(
        &child.id,
        1,
        "RuntimeJobCompleted",
        "runtime-worker",
    )
    .with_payload(json!({
        "runtime_job_id": "child-inspection-job",
        "activity_result": {
            "activity": super::super::PR_FEEDBACK_INSPECT_ACTIVITY,
            "status": "succeeded",
            "summary": "Feedback remains.",
            "artifacts": [],
            "signals": [],
            "validation": [],
            "error": null,
            "error_kind": null
        }
    }));

    let payload = merge_child_completion_payload(&event, &child);

    assert_eq!(payload["child_workflow_id"], child.id);
    assert_eq!(payload["recovery_activity"], "start_child_workflow");
    assert_eq!(payload["recovery_runtime_job_id"], "parent-start-child-job");
    assert_eq!(payload["runtime_job_id"], "child-inspection-job");
}

#[tokio::test]
async fn preflight_result_completes_job_before_runtime_turn_starts() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&harness_core::config::dirs::default_db_path(
        dir.path(),
        "workflow_runtime",
    ))
    .await?;
    let job = enqueue_test_runtime_job(
        &store,
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;
    let executions = Arc::new(AtomicUsize::new(0));
    let executor = PreflightRuntimeExecutor {
        result: ActivityResult::cancelled("check", "Runtime worker disabled."),
        executions: executions.clone(),
    };

    let completed = RuntimeWorker::new(&store, "runtime-1")
        .with_lease_ttl(Duration::minutes(5))
        .run_once(&executor)
        .await?
        .ok_or_else(|| anyhow::anyhow!("worker should complete the claimed job"))?;

    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Cancelled);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let events = store.runtime_events_for(&completed.id).await?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[1].event_type, "ActivityResultReady");
    Ok(())
}

#[tokio::test]
async fn claim_guard_defers_job_before_runtime_dispatch() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = match WorkflowRuntimeStore::open(&harness_core::config::dirs::default_db_path(
        dir.path(),
        "workflow_runtime",
    ))
    .await
    {
        Ok(store) => store,
        Err(error) => {
            tracing::warn!("runtime worker claim guard test skipped: {error}");
            return Ok(());
        }
    };
    let job = enqueue_test_runtime_job(
        &store,
        "guard-defer",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;
    let not_before = Utc::now() + Duration::minutes(10);
    let guard = DeferringClaimGuard { not_before };
    let executions = Arc::new(AtomicUsize::new(0));
    let executor = PreflightRuntimeExecutor {
        result: ActivityResult::succeeded("check", "should not run"),
        executions: executions.clone(),
    };

    let completed = RuntimeWorker::new(&store, "runtime-1")
        .with_lease_ttl(Duration::minutes(5))
        .with_claim_guard(&guard)
        .run_once(&executor)
        .await?;

    assert!(completed.is_none());
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    let deferred = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should still exist"))?;
    assert_eq!(deferred.status, RuntimeJobStatus::Pending);
    assert!(deferred.lease.is_none());
    assert_eq!(deferred.not_before, Some(not_before));
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "RuntimeJobClaimDeferred");
    assert_eq!(events[0].event["reason"], "test guard");
    Ok(())
}

#[tokio::test]
async fn runtime_worker_skips_remote_host_jobs_for_external_claims() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&harness_core::config::dirs::default_db_path(
        dir.path(),
        "workflow_runtime",
    ))
    .await?;
    let remote_job = enqueue_test_runtime_job(
        &store,
        "command-remote",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let local_job = enqueue_test_runtime_job(
        &store,
        "command-local",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "local_check" }),
    )
    .await?;
    let executions = Arc::new(AtomicUsize::new(0));
    let executor = PreflightRuntimeExecutor {
        result: ActivityResult::succeeded("local_check", "Local worker completed."),
        executions,
    };

    let completed = RuntimeWorker::new(&store, "runtime-1")
        .with_lease_ttl(Duration::minutes(5))
        .run_once(&executor)
        .await?
        .ok_or_else(|| anyhow::anyhow!("worker should claim the local job"))?;

    assert_eq!(completed.id, local_job.id);
    let remote = store
        .get_runtime_job(&remote_job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("remote job should remain pending for runtime host API"))?;
    assert_eq!(remote.status, RuntimeJobStatus::Pending);
    Ok(())
}

#[test]
fn mark_failed_inline_command_persists_failure_reason_into_data() {
    let mut instance = prompt_task_instance();
    let command = WorkflowCommand::new(
        WorkflowCommandType::MarkFailed,
        "runtime-completion:evt-1:failed",
        json!({ "reason": "Agent turn timed out after 900s", "error_kind": "timeout", "retry_hint": "Retry after route repair.", "last_stop": {"state": "failed", "runtime_job_id": "job-1"} }),
    );

    apply_failure_reason_side_effect(&mut instance, &command).unwrap();

    assert_eq!(
        instance.data.get("failure_reason").and_then(Value::as_str),
        Some("Agent turn timed out after 900s"),
        "MarkFailed must surface its reason as the queryable failure_reason"
    );
    assert_eq!(
        instance
            .data_provenance
            .as_ref()
            .and_then(|provenance| provenance.provenance_for("/failure_reason")),
        Some(super::super::DataProvenance::Agent)
    );
    assert_eq!(instance.data["error_kind"], "timeout");
    assert_eq!(instance.data["retry_hint"], "Retry after route repair.");
    assert_eq!(instance.data["last_stop"]["runtime_job_id"], "job-1");
}

#[test]
fn mark_blocked_inline_command_persists_stop_metadata_into_data() {
    let mut instance = prompt_task_instance();
    let command = WorkflowCommand::new(
        WorkflowCommandType::MarkBlocked,
        "runtime-completion:evt-2:blocked",
        json!({"reason": "Waiting for maintainer approval.", "unblock_hint": "Post approval, then call unblock.", "last_stop": {"state": "blocked", "runtime_job_id": "job-2"}}),
    );

    apply_failure_reason_side_effect(&mut instance, &command).unwrap();

    assert_eq!(
        instance.data["blocked_reason"],
        "Waiting for maintainer approval."
    );
    assert_eq!(
        instance.data["unblock_hint"],
        "Post approval, then call unblock."
    );
    assert_eq!(
        instance
            .data_provenance
            .as_ref()
            .and_then(|provenance| provenance.provenance_for("/blocked_reason")),
        Some(super::super::DataProvenance::Agent)
    );
    assert_eq!(instance.data["last_stop"]["runtime_job_id"], "job-2");
}

fn prompt_task_instance() -> WorkflowInstance {
    WorkflowInstance::new(
        "prompt_task",
        1,
        "implementing",
        WorkflowSubject::new("p", "1"),
    )
}

struct LeaseLostExecutor {
    cancel_calls: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RuntimeJobExecutor for LeaseLostExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        self.release.notified().await;
        ActivityResult::cancelled("check", "cancelled after lease lost")
    }

    async fn cancel_execution(&self, _job: &RuntimeJob) {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

#[tokio::test]
async fn lease_lost_cancels_execution_and_waits_for_cleanup() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let job = enqueue_test_runtime_job(
        &store,
        "lease-lost-cancel",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "check" }),
    )
    .await?;

    let cancel_calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let executor = LeaseLostExecutor {
        cancel_calls: Arc::clone(&cancel_calls),
        release: Arc::clone(&release),
    };
    // Short lease so the renewal loop hits the tampered lease quickly; the
    // tamper task steals ownership while execute is still blocked.
    let worker =
        RuntimeWorker::new(&store, "lease-lost-worker").with_lease_ttl(Duration::seconds(4));
    let pool = store.pool().clone();
    let tamper_job_id = job.id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        sqlx::query(
            r#"UPDATE runtime_jobs
               SET data = jsonb_set(data, '{lease,owner}', '"other-worker"')
               WHERE id = $1"#,
        )
        .bind(&tamper_job_id)
        .execute(&pool)
        .await
        .expect("lease tamper should apply");
    });

    let completed = worker.run_once(&executor).await?;

    assert_eq!(
        cancel_calls.load(Ordering::SeqCst),
        1,
        "executor cancel must be invoked"
    );
    assert!(completed.is_none(), "lease-lost completion must not commit");

    let (dlq_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(
        dlq_count, 1,
        "lease-lost result must land in the dead-letter"
    );
    Ok(())
}

struct DeadlineExecutor {
    deadline: StdDuration,
}

#[async_trait]
impl RuntimeJobExecutor for DeadlineExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        tokio::time::sleep(self.deadline).await;
        ActivityResult::failed("check", "execution deadline observed", "turn timed out")
    }
}

struct GatedCancellationExecutor {
    cancelled: tokio::sync::watch::Sender<bool>,
    cleanup_started: tokio::sync::Notify,
    cleanup_gate: tokio::sync::Semaphore,
}

impl GatedCancellationExecutor {
    fn new() -> Self {
        Self {
            cancelled: tokio::sync::watch::channel(false).0,
            cleanup_started: tokio::sync::Notify::new(),
            cleanup_gate: tokio::sync::Semaphore::new(0),
        }
    }
}

#[async_trait]
impl RuntimeJobExecutor for GatedCancellationExecutor {
    async fn execute(&self, _job: RuntimeJob) -> ActivityResult {
        let mut cancelled = self.cancelled.subscribe();
        while !*cancelled.borrow() {
            cancelled
                .changed()
                .await
                .expect("cancellation sender remains alive");
        }
        self.cleanup_started.notify_one();
        self.cleanup_gate
            .acquire()
            .await
            .expect("cleanup gate remains alive")
            .forget();
        ActivityResult::cancelled("check", "execution drained after lease loss")
    }

    async fn cancel_execution(&self, _job: &RuntimeJob) {
        self.cancelled.send_replace(true);
    }
}

fn lazy_store_for_stalled_connection(port: u16) -> WorkflowRuntimeStore {
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("lease-test")
        .database("lease-test")
        .ssl_mode(sqlx::postgres::PgSslMode::Disable);
    WorkflowRuntimeStore {
        pool: sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(StdDuration::from_secs(20))
            .connect_lazy_with(options),
        definition_registry: crate::runtime::WorkflowDefinitionRegistry::with_builtins()
            .into_shared(),
        budget_policy: harness_core::config::workflow::RuntimeBudgetPolicy::default(),
    }
}

#[tokio::test]
async fn lease_renewal_io_keeps_polling_execution_deadline() -> anyhow::Result<()> {
    // Exercise the real SQLx acquisition path against a TCP peer which accepts
    // the connection and deliberately never answers the PostgreSQL handshake.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let store = lazy_store_for_stalled_connection(listener.local_addr()?.port());
    let worker = RuntimeWorker::new(&store, "stalled-db").with_lease_ttl(Duration::seconds(4));
    let job = RuntimeJob::pending("test-command", RuntimeKind::CodexJsonrpc, "test", json!({}));
    let executor = DeadlineExecutor {
        deadline: StdDuration::from_millis(2500),
    };
    let run = worker.execute_with_lease_renewal(&job, &executor, Utc::now() + Duration::seconds(4));
    tokio::pin!(run);
    let socket = tokio::select! {
        accepted = listener.accept() => accepted?.0,
        result = &mut run => panic!("execution completed before starting renewal: {}", result.is_ok()),
    };
    let outcome = tokio::time::timeout(StdDuration::from_secs(1), &mut run).await??;
    assert_eq!(outcome.result.summary, "execution deadline observed");
    drop(socket);
    store.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn lease_renewal_io_expires_and_waits_for_cancellation_acknowledgement() -> anyhow::Result<()>
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let store = lazy_store_for_stalled_connection(listener.local_addr()?.port());
    let worker = RuntimeWorker::new(&store, "stalled-db").with_lease_ttl(Duration::seconds(2));
    let job = RuntimeJob::pending("test-command", RuntimeKind::CodexJsonrpc, "test", json!({}));
    let executor = GatedCancellationExecutor::new();
    let run = worker.execute_with_lease_renewal(&job, &executor, Utc::now() + Duration::seconds(2));
    tokio::pin!(run);
    let socket = tokio::select! {
        accepted = listener.accept() => accepted?.0,
        _ = &mut run => panic!("execution must remain active until cancellation"),
    };
    tokio::select! {
        reached = tokio::time::timeout(StdDuration::from_secs(2), executor.cleanup_started.notified()) => reached?,
        _ = &mut run => panic!("execution must not return before cleanup acknowledges"),
    }
    assert!(tokio::time::timeout(StdDuration::from_millis(30), &mut run)
        .await
        .is_err());
    executor.cleanup_gate.add_permits(1);
    let outcome = tokio::time::timeout(StdDuration::from_secs(1), &mut run).await??;
    assert_eq!(outcome.result.status, ActivityStatus::Cancelled);
    assert!(outcome.result.artifacts.iter().any(|artifact| {
        artifact.artifact_type == "runtime_job_lease_renewal_failure"
            && artifact.artifact["error"]
                .as_str()
                .is_some_and(|error| error.contains("lease expired"))
    }));
    drop(socket);
    store.pool.close().await;
    Ok(())
}

#[tokio::test]
async fn lease_renewal_io_error_waits_for_cancellation_acknowledgement() -> anyhow::Result<()> {
    let store = lazy_store_for_stalled_connection(1);
    store.pool.close().await;
    let worker = RuntimeWorker::new(&store, "closed-db").with_lease_ttl(Duration::seconds(4));
    let job = RuntimeJob::pending("test-command", RuntimeKind::CodexJsonrpc, "test", json!({}));
    let executor = GatedCancellationExecutor::new();
    let run = worker.execute_with_lease_renewal(&job, &executor, Utc::now() + Duration::seconds(4));
    tokio::pin!(run);
    tokio::select! {
        reached = tokio::time::timeout(StdDuration::from_secs(3), executor.cleanup_started.notified()) => reached?,
        _ = &mut run => panic!("a database error must not bypass execution cleanup"),
    }
    assert!(tokio::time::timeout(StdDuration::from_millis(30), &mut run)
        .await
        .is_err());
    executor.cleanup_gate.add_permits(1);
    let outcome = tokio::time::timeout(StdDuration::from_secs(1), &mut run).await??;
    assert_eq!(outcome.result.status, ActivityStatus::Cancelled);
    assert!(outcome.result.artifacts.iter().any(|artifact| {
        artifact.artifact_type == "runtime_job_lease_renewal_failure"
            && artifact.artifact["error"]
                .as_str()
                .is_some_and(|error| error.contains("closed pool"))
    }));
    Ok(())
}

#[tokio::test]
async fn lease_renewal_row_lock_does_not_delay_execution_deadline() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "renewal-row-lock",
        RuntimeKind::CodexJsonrpc,
        "test",
        json!({"activity":"check"}),
    )
    .await?;
    let expires_at = Utc::now() + Duration::seconds(4);
    let job = store
        .claim_next_runtime_job_excluding_runtime_kind(
            RuntimeKind::RemoteHost,
            "row-lock-worker",
            expires_at,
        )
        .await?
        .expect("test job is claimed");
    let mut blocker = store.pool().begin().await?;
    sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE")
        .bind(&job.id)
        .fetch_one(&mut *blocker)
        .await?;
    let worker = RuntimeWorker::new(&store, "row-lock-worker").with_lease_ttl(Duration::seconds(4));
    let executor = DeadlineExecutor {
        deadline: StdDuration::from_millis(2500),
    };
    let outcome = tokio::time::timeout(
        StdDuration::from_secs(3),
        worker.execute_with_lease_renewal(&job, &executor, expires_at),
    )
    .await??;
    assert_eq!(outcome.result.summary, "execution deadline observed");
    blocker.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn lease_renewal_row_lock_expires_without_resurrecting_the_lease() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "renewal-row-lock-expiry",
        RuntimeKind::CodexJsonrpc,
        "test",
        json!({"activity":"check"}),
    )
    .await?;
    let expires_at = Utc::now() + Duration::seconds(2);
    let job = store
        .claim_next_runtime_job_excluding_runtime_kind(
            RuntimeKind::RemoteHost,
            "row-lock-worker",
            expires_at,
        )
        .await?
        .expect("test job is claimed");
    let mut blocker = store.pool().begin().await?;
    sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE")
        .bind(&job.id)
        .fetch_one(&mut *blocker)
        .await?;
    let worker = RuntimeWorker::new(&store, "row-lock-worker").with_lease_ttl(Duration::seconds(2));
    let executor = GatedCancellationExecutor::new();
    let run = worker.execute_with_lease_renewal(&job, &executor, expires_at);
    tokio::pin!(run);
    tokio::select! {
        reached = tokio::time::timeout(StdDuration::from_secs(3), executor.cleanup_started.notified()) => reached?,
        _ = &mut run => panic!("cleanup must remain owned while its gate is closed"),
    }
    assert!(tokio::time::timeout(StdDuration::from_millis(30), &mut run)
        .await
        .is_err());
    executor.cleanup_gate.add_permits(1);
    let outcome = tokio::time::timeout(StdDuration::from_secs(1), &mut run).await??;
    assert_eq!(outcome.result.status, ActivityStatus::Cancelled);
    assert!(outcome
        .result
        .artifacts
        .iter()
        .any(|artifact| { artifact.artifact_type == "runtime_job_lease_renewal_failure" }));
    blocker.rollback().await?;
    let current = store
        .get_runtime_job(&job.id)
        .await?
        .expect("job remains available");
    assert_eq!(
        current.lease.as_ref().expect("original lease").expires_at,
        expires_at
    );
    let completion = store
        .commit_runtime_activity_completion_with_transcript_if_owned(
            &job.id,
            "row-lock-worker",
            expires_at,
            &ActivityResult::succeeded("check", "stale completion"),
            None,
        )
        .await?;
    assert!(
        completion.is_none(),
        "expired ownership cannot publish completion"
    );
    Ok(())
}

#[tokio::test]
async fn lease_renewal_statement_timeout_releases_a_blocked_transaction() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "renewal-statement-timeout",
        RuntimeKind::CodexJsonrpc,
        "test",
        json!({"activity":"check"}),
    )
    .await?;
    let expires_at = Utc::now() + Duration::seconds(2);
    let job = store
        .claim_next_runtime_job_excluding_runtime_kind(
            RuntimeKind::RemoteHost,
            "statement-timeout-worker",
            expires_at,
        )
        .await?
        .expect("test job is claimed");
    let mut blocker = store.pool().begin().await?;
    sqlx::query("SELECT id FROM runtime_jobs WHERE id = $1 FOR UPDATE")
        .bind(&job.id)
        .fetch_one(&mut *blocker)
        .await?;
    let renewal = store.extend_runtime_job_lease_if_owned(
        &job.id,
        "statement-timeout-worker",
        expires_at,
        expires_at + Duration::seconds(2),
    );
    let error = tokio::time::timeout(StdDuration::from_secs(3), renewal)
        .await?
        .expect_err("PostgreSQL must bound its own row-lock wait");
    let database_error = error
        .downcast_ref::<sqlx::Error>()
        .and_then(sqlx::Error::as_database_error)
        .expect("statement timeout is a PostgreSQL error");
    assert_eq!(database_error.code().as_deref(), Some("57014"));
    blocker.rollback().await?;
    let current = store.get_runtime_job(&job.id).await?.expect("original job");
    assert_eq!(
        current.lease.expect("original lease").expires_at,
        expires_at
    );
    Ok(())
}

#[tokio::test]
async fn lease_renewal_success_keeps_long_execution_owned() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    enqueue_test_runtime_job(
        &store,
        "renewal-success",
        RuntimeKind::CodexJsonrpc,
        "test",
        json!({"activity":"check"}),
    )
    .await?;
    let expires_at = Utc::now() + Duration::seconds(2);
    let job = store
        .claim_next_runtime_job_excluding_runtime_kind(
            RuntimeKind::RemoteHost,
            "renewal-success-worker",
            expires_at,
        )
        .await?
        .expect("test job is claimed");
    let worker =
        RuntimeWorker::new(&store, "renewal-success-worker").with_lease_ttl(Duration::seconds(2));
    let executor = DeadlineExecutor {
        deadline: StdDuration::from_millis(2500),
    };
    let outcome = tokio::time::timeout(
        StdDuration::from_secs(4),
        worker.execute_with_lease_renewal(&job, &executor, expires_at),
    )
    .await??;
    assert!(outcome.lease_expires_at > expires_at);
    assert_eq!(outcome.result.summary, "execution deadline observed");
    Ok(())
}
