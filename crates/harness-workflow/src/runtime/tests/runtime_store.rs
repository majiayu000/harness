use super::*;

#[tokio::test]
async fn runtime_jobs_accept_typed_status_values() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let statuses = [
        RuntimeJobStatus::Pending,
        RuntimeJobStatus::Running,
        RuntimeJobStatus::Succeeded,
        RuntimeJobStatus::Failed,
        RuntimeJobStatus::Cancelled,
    ];

    for (index, status) in statuses.iter().copied().enumerate() {
        let status = runtime_job_status_str(status)?;
        insert_runtime_job_with_status(&store, &format!("typed-status-{index}"), &status).await?;
    }

    let (persisted_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM runtime_jobs")
        .fetch_one(store.pool())
        .await?;
    assert_eq!(persisted_count, statuses.len() as i64);
    Ok(())
}

#[tokio::test]
async fn runtime_jobs_reject_legacy_invalid_status_values() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let error = insert_runtime_job_with_status(&store, "legacy-expired-status", "expired")
        .await
        .expect_err("expired runtime job status should be rejected");
    let error = format!("{error:#}");
    assert!(
        error.contains("runtime_jobs_status_check"),
        "unexpected persistence error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_jobs_migration_rewrites_legacy_expired_statuses() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("workflow_runtime.db");
    let store = WorkflowRuntimeStore::open(&db_path).await?;
    let job = store
        .enqueue_runtime_job(
            "legacy-expired-command",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"legacy": true}),
        )
        .await?;
    sqlx::query("ALTER TABLE runtime_jobs DROP CONSTRAINT IF EXISTS runtime_jobs_status_check")
        .execute(store.pool())
        .await?;
    sqlx::query(
        "ALTER TABLE runtime_jobs
            ADD CONSTRAINT runtime_jobs_status_check
            CHECK (status IN (
                'pending', 'running', 'succeeded',
                'failed', 'cancelled', 'expired'
            ))",
    )
    .execute(store.pool())
    .await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 13")
        .execute(store.pool())
        .await?;
    sqlx::query(
        "UPDATE runtime_jobs
         SET status = 'expired',
             data = jsonb_set(data, '{status}', '\"expired\"'::jsonb, false)
         WHERE id = $1",
    )
    .bind(&job.id)
    .execute(store.pool())
    .await?;
    drop(store);

    let store = WorkflowRuntimeStore::open(&db_path).await?;
    let migrated_job = store
        .get_runtime_job(&job.id)
        .await?
        .expect("legacy expired job should remain readable after migration");
    assert_eq!(migrated_job.status, RuntimeJobStatus::Failed);
    let (rewritten_status, payload_status): (String, String) =
        sqlx::query_as("SELECT status, data->>'status' FROM runtime_jobs WHERE id = $1")
            .bind(&job.id)
            .fetch_one(store.pool())
            .await?;
    assert_eq!(rewritten_status, "failed");
    assert_eq!(payload_status, "failed");

    let error = insert_runtime_job_with_status(&store, "legacy-expired-after-migration", "expired")
        .await
        .expect_err("expired runtime job status should be rejected after migration");
    let error = format!("{error:#}");
    assert!(
        error.contains("runtime_jobs_status_check"),
        "unexpected persistence error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn nonterminal_listing_uses_definition_specific_terminal_states() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let issue_passed = project_issue_instance("/project-a", 223, "passed");
    let issue_done = project_issue_instance("/project-a", 224, "done");
    let quality_checking = quality_gate_instance("checking")
        .with_id("/project-a::quality:checking")
        .with_data(json!({ "project_id": "/project-a" }));
    let quality_passed = quality_gate_instance("passed")
        .with_id("/project-a::quality:passed")
        .with_data(json!({ "project_id": "/project-a" }));
    store.upsert_instance(&issue_passed).await?;
    store.upsert_instance(&issue_done).await?;
    store.upsert_instance(&quality_checking).await?;
    store.upsert_instance(&quality_passed).await?;

    let issue_ids: std::collections::HashSet<_> = store
        .list_nonterminal_instances_by_definition(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            Some("/project-a"),
            None,
        )
        .await?
        .into_iter()
        .map(|instance| instance.id)
        .collect();
    assert!(issue_ids.contains(&issue_passed.id));
    assert!(!issue_ids.contains(&issue_done.id));

    let quality_ids: std::collections::HashSet<_> = store
        .list_nonterminal_instances_by_definition(
            QUALITY_GATE_DEFINITION_ID,
            Some("/project-a"),
            None,
        )
        .await?
        .into_iter()
        .map(|instance| instance.id)
        .collect();
    assert!(quality_ids.contains(&quality_checking.id));
    assert!(!quality_ids.contains(&quality_passed.id));
    Ok(())
}

fn runtime_job_status_str(status: RuntimeJobStatus) -> anyhow::Result<String> {
    let value = serde_json::to_value(status)?;
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("runtime job status did not serialize to a string"))
}

async fn insert_runtime_job_with_status(
    store: &WorkflowRuntimeStore,
    id: &str,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(id)
    .bind(format!("command-{id}"))
    .bind("codex_jsonrpc")
    .bind("codex-default")
    .bind(status)
    .bind("{}")
    .execute(store.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn dedupe_uses_runtime_job_timestamps_not_uuid_order() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = project_issue_instance("/project-a", 123, "replanning");
    store.upsert_instance(&instance).await?;
    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-latest");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let older = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"attempt": 1}),
        )
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let newer = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"attempt": 2}),
        )
        .await?;
    assert!(older.created_at < newer.created_at);

    let shared_db_created_at = older.created_at;
    sqlx::query("UPDATE runtime_jobs SET id = $1, created_at = $2 WHERE id = $3")
        .bind("ffffffff-ffff-4fff-bfff-ffffffffffff")
        .bind(shared_db_created_at)
        .bind(&older.id)
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE runtime_jobs SET id = $1, created_at = $2 WHERE id = $3")
        .bind("00000000-0000-4000-8000-000000000000")
        .bind(shared_db_created_at)
        .bind(&newer.id)
        .execute(store.pool())
        .await?;

    let outcome = store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"attempt": 3}),
            None,
        )
        .await?;

    let jobs = store
        .runtime_jobs_for_commands_limited(std::slice::from_ref(&command_id), 1)
        .await?;
    assert_eq!(jobs[&command_id][0].id, newer.id);

    assert_ne!(older.id, newer.id);
    match outcome {
        RuntimeJobEnqueueOutcome::AlreadyExists(job) => assert_eq!(job.id, newer.id),
        other => panic!("unexpected latest-job dedupe outcome: {other:?}"),
    }
    Ok(())
}
