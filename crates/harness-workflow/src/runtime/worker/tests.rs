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
    store.upsert_instance(&workflow).await?;
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
