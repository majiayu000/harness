use super::{
    RuntimeJob, RuntimeJobStatus, RuntimeKind, WorkflowCommand, WorkflowInstance,
    WorkflowRuntimeStore, WorkflowSubject,
};
use chrono::{Duration, Utc};
use harness_core::db::resolve_database_url;
use serde_json::json;

async fn open_breaker_test_store() -> anyhow::Result<Option<WorkflowRuntimeStore>> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(None);
    };
    let dir = tempfile::tempdir()?;
    match WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await {
        Ok(store) => Ok(Some(store)),
        Err(error) => {
            tracing::warn!("runtime breaker store test skipped: {error}");
            Ok(None)
        }
    }
}

async fn enqueue_breaker_test_job(
    store: &WorkflowRuntimeStore,
    command_key: &str,
    runtime_profile: &str,
    not_before: Option<chrono::DateTime<Utc>>,
) -> anyhow::Result<RuntimeJob> {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", format!("issue:{command_key}")),
    )
    .with_id(format!("breaker-test-workflow-{command_key}"));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("check", format!("breaker-{command_key}"));
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
        .enqueue_runtime_job_with_not_before(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            runtime_profile,
            json!({ "activity": "check" }),
            not_before,
        )
        .await
}

#[tokio::test]
async fn runtime_store_records_failure_class_in_job_data() -> anyhow::Result<()> {
    let Some(store) = open_breaker_test_store().await? else {
        return Ok(());
    };
    let job = enqueue_breaker_test_job(&store, "failure-class", "codex-default", None).await?;

    let updated = store
        .record_runtime_job_failure_class(&job.id, "quota-interactive-wait")
        .await?
        .expect("runtime job should be updated");

    assert_eq!(
        updated.failure_class.as_deref(),
        Some("quota-interactive-wait")
    );
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should be persisted");
    assert_eq!(
        persisted.failure_class.as_deref(),
        Some("quota-interactive-wait")
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_defers_ready_runtime_jobs_for_profile() -> anyhow::Result<()> {
    let Some(store) = open_breaker_test_store().await? else {
        return Ok(());
    };
    let first = enqueue_breaker_test_job(&store, "first", "codex-default", None).await?;
    let second = enqueue_breaker_test_job(&store, "second", "codex-default", None).await?;
    let other = enqueue_breaker_test_job(&store, "other", "claude-default", None).await?;
    let already_deferred = enqueue_breaker_test_job(
        &store,
        "already-deferred",
        "codex-default",
        Some(Utc::now() + Duration::minutes(30)),
    )
    .await?;
    let cooldown_until = Utc::now() + Duration::minutes(10);

    let deferred = store
        .defer_ready_runtime_jobs_for_profile("codex-default", cooldown_until)
        .await?;

    assert_eq!(deferred, 2);
    assert_eq!(
        store
            .get_runtime_job(&first.id)
            .await?
            .expect("first job should exist")
            .not_before,
        Some(cooldown_until)
    );
    assert_eq!(
        store
            .get_runtime_job(&second.id)
            .await?
            .expect("second job should exist")
            .not_before,
        Some(cooldown_until)
    );
    assert!(store
        .get_runtime_job(&other.id)
        .await?
        .expect("other profile job should exist")
        .not_before
        .is_none());
    assert_ne!(
        store
            .get_runtime_job(&already_deferred.id)
            .await?
            .expect("deferred job should exist")
            .not_before,
        Some(cooldown_until)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_store_defers_owned_claim_back_to_pending() -> anyhow::Result<()> {
    let Some(store) = open_breaker_test_store().await? else {
        return Ok(());
    };
    let job = enqueue_breaker_test_job(&store, "claimed-defer", "codex-default", None).await?;
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job("worker-a", lease_expires_at)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should be claimed"))?;
    assert_eq!(claimed.id, job.id);
    let not_before = Utc::now() + Duration::minutes(10);

    let deferred = store
        .defer_runtime_job_claim_if_owned(&job.id, "worker-a", lease_expires_at, not_before)
        .await?
        .ok_or_else(|| anyhow::anyhow!("current lease should be deferrable"))?;

    assert_eq!(deferred.status, RuntimeJobStatus::Pending);
    assert!(deferred.lease.is_none());
    assert_eq!(deferred.not_before, Some(not_before));
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should still exist"))?;
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert!(persisted.lease.is_none());
    assert_eq!(persisted.not_before, Some(not_before));

    let stale_attempt = store
        .defer_runtime_job_claim_if_owned(&job.id, "worker-a", lease_expires_at, Utc::now())
        .await?;
    assert!(stale_attempt.is_none());
    Ok(())
}
