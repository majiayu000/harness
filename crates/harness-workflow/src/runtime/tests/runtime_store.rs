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
async fn runtime_graph_rejects_orphan_references() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = issue_instance("implementing").with_id("fk-workflow");
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("fk_activity", "fk-command");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "fk_activity" }),
        )
        .await?;

    let error = sqlx::query(
        "INSERT INTO workflow_events (id, workflow_id, sequence, event_type, source, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind("orphan-event")
    .bind("missing-workflow")
    .bind(1_i64)
    .bind("OrphanEvent")
    .bind("test")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow event without a workflow should be rejected");
    assert_constraint_error(error, "workflow_events_workflow_id_fkey");

    let error = sqlx::query(
        "INSERT INTO workflow_decisions (id, workflow_id, event_id, accepted, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("orphan-decision-workflow")
    .bind("missing-workflow")
    .bind(Option::<String>::None)
    .bind(true)
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow decision without a workflow should be rejected");
    assert_constraint_error(error, "workflow_decisions_workflow_id_fkey");

    let error = sqlx::query(
        "INSERT INTO workflow_decisions (id, workflow_id, event_id, accepted, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("orphan-decision-event")
    .bind(&workflow.id)
    .bind("missing-event")
    .bind(true)
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow decision without an event should be rejected");
    assert_constraint_error(error, "workflow_decisions_event_id_fkey");

    let error = sqlx::query(
        "INSERT INTO workflow_commands
            (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
    )
    .bind("orphan-command-workflow")
    .bind("missing-workflow")
    .bind(Option::<String>::None)
    .bind("enqueue_activity")
    .bind("orphan-command-workflow")
    .bind("pending")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow command without a workflow should be rejected");
    assert_constraint_error(error, "workflow_commands_workflow_id_fkey");

    let error = sqlx::query(
        "INSERT INTO workflow_commands
            (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
    )
    .bind("orphan-command-decision")
    .bind(&workflow.id)
    .bind("missing-decision")
    .bind("enqueue_activity")
    .bind("orphan-command-decision")
    .bind("pending")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow command without a decision should be rejected");
    assert_constraint_error(error, "workflow_commands_decision_id_fkey");

    let error = sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind("orphan-runtime-job")
    .bind("missing-command")
    .bind("codex_jsonrpc")
    .bind("codex-default")
    .bind("pending")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("runtime job without a command should be rejected");
    assert_constraint_error(error, "runtime_jobs_command_id_fkey");

    let error = sqlx::query(
        "INSERT INTO runtime_events (id, runtime_job_id, sequence, event_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("orphan-runtime-event")
    .bind("missing-runtime-job")
    .bind(1_i64)
    .bind("RuntimePromptPrepared")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("runtime event without a runtime job should be rejected");
    assert_constraint_error(error, "runtime_events_runtime_job_id_fkey");

    let error = sqlx::query(
        "INSERT INTO workflow_artifacts (id, workflow_id, runtime_job_id, artifact_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("orphan-artifact-workflow")
    .bind("missing-workflow")
    .bind(Option::<String>::None)
    .bind("test")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow artifact without a workflow should be rejected");
    assert_constraint_error(error, "workflow_artifacts_workflow_id_fkey");

    let error = sqlx::query(
        "INSERT INTO workflow_artifacts (id, workflow_id, runtime_job_id, artifact_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("orphan-artifact-runtime-job")
    .bind(&workflow.id)
    .bind("missing-runtime-job")
    .bind("test")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("workflow artifact without a runtime job should be rejected");
    assert_constraint_error(error, "workflow_artifacts_runtime_job_id_fkey");

    store
        .record_runtime_event(
            &runtime_job.id,
            "RuntimePromptPrepared",
            json!({ "ok": true }),
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn insert_instance_if_absent_does_not_overwrite_existing_workflow() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow_id = "insert-if-absent-existing-workflow";
    let existing = issue_instance("implementing")
        .with_id(workflow_id)
        .with_data(json!({"marker": "real"}));
    store.upsert_instance(&existing).await?;

    let fallback = issue_instance("failed")
        .with_id(workflow_id)
        .with_data(json!({"marker": "fallback"}));
    let inserted = store.insert_instance_if_absent(&fallback).await?;

    assert!(!inserted);
    let Some(persisted) = store.get_instance(workflow_id).await? else {
        anyhow::bail!("existing workflow should still be present");
    };
    assert_eq!(persisted.state, "implementing");
    assert_eq!(persisted.data["marker"], "real");

    let new_workflow = issue_instance("failed").with_id("insert-if-absent-new-workflow");
    assert!(store.insert_instance_if_absent(&new_workflow).await?);
    Ok(())
}

#[tokio::test]
async fn runtime_graph_fk_migration_allows_existing_orphan_rows() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("workflow_runtime.db");
    let store = WorkflowRuntimeStore::open(&db_path).await?;
    drop_runtime_graph_fk_constraints(&store).await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 14")
        .execute(store.pool())
        .await?;
    insert_legacy_orphan_runtime_graph_rows(&store).await?;
    drop(store);

    let store = WorkflowRuntimeStore::open(&db_path).await?;
    let (orphan_event_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM workflow_events WHERE id = $1")
            .bind("legacy-orphan-event")
            .fetch_one(store.pool())
            .await?;
    assert_eq!(orphan_event_count, 1);

    let error = sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind("new-orphan-runtime-job-after-v14")
    .bind("missing-command-after-v14")
    .bind("codex_jsonrpc")
    .bind("codex-default")
    .bind("pending")
    .bind("{}")
    .execute(store.pool())
    .await
    .expect_err("v14 should reject new orphan runtime jobs after migration");
    assert_constraint_error(error, "runtime_jobs_command_id_fkey");
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
    let job = enqueue_test_runtime_job(
        &store,
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

fn assert_constraint_error(error: sqlx::Error, constraint_name: &str) {
    let error = format!("{error:#}");
    assert!(
        error.contains(constraint_name),
        "expected {constraint_name}, got: {error}"
    );
}

async fn drop_runtime_graph_fk_constraints(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    for statement in [
        "ALTER TABLE workflow_events DROP CONSTRAINT IF EXISTS workflow_events_workflow_id_fkey",
        "ALTER TABLE workflow_decisions DROP CONSTRAINT IF EXISTS workflow_decisions_workflow_id_fkey",
        "ALTER TABLE workflow_decisions DROP CONSTRAINT IF EXISTS workflow_decisions_event_id_fkey",
        "ALTER TABLE workflow_commands DROP CONSTRAINT IF EXISTS workflow_commands_workflow_id_fkey",
        "ALTER TABLE workflow_commands DROP CONSTRAINT IF EXISTS workflow_commands_decision_id_fkey",
        "ALTER TABLE runtime_jobs DROP CONSTRAINT IF EXISTS runtime_jobs_command_id_fkey",
        "ALTER TABLE runtime_events DROP CONSTRAINT IF EXISTS runtime_events_runtime_job_id_fkey",
        "ALTER TABLE workflow_artifacts DROP CONSTRAINT IF EXISTS workflow_artifacts_workflow_id_fkey",
        "ALTER TABLE workflow_artifacts DROP CONSTRAINT IF EXISTS workflow_artifacts_runtime_job_id_fkey",
    ] {
        sqlx::query(statement).execute(store.pool()).await?;
    }
    Ok(())
}

async fn insert_legacy_orphan_runtime_graph_rows(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO workflow_events (id, workflow_id, sequence, event_type, source, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind("legacy-orphan-event")
    .bind("missing-legacy-workflow")
    .bind(1_i64)
    .bind("LegacyOrphanEvent")
    .bind("test")
    .bind("{}")
    .execute(store.pool())
    .await?;

    sqlx::query(
        "INSERT INTO workflow_decisions (id, workflow_id, event_id, accepted, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("legacy-orphan-decision")
    .bind("missing-legacy-workflow")
    .bind("missing-legacy-event")
    .bind(true)
    .bind("{}")
    .execute(store.pool())
    .await?;

    sqlx::query(
        "INSERT INTO workflow_commands
            (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
    )
    .bind("legacy-orphan-command")
    .bind("missing-legacy-workflow")
    .bind("missing-legacy-decision")
    .bind("enqueue_activity")
    .bind("legacy-orphan-command")
    .bind("pending")
    .bind("{}")
    .execute(store.pool())
    .await?;

    sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind("legacy-orphan-runtime-job")
    .bind("missing-legacy-command")
    .bind("codex_jsonrpc")
    .bind("codex-default")
    .bind("pending")
    .bind("{}")
    .execute(store.pool())
    .await?;

    sqlx::query(
        "INSERT INTO runtime_events (id, runtime_job_id, sequence, event_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("legacy-orphan-runtime-event")
    .bind("missing-legacy-runtime-job")
    .bind(1_i64)
    .bind("RuntimePromptPrepared")
    .bind("{}")
    .execute(store.pool())
    .await?;

    sqlx::query(
        "INSERT INTO workflow_artifacts (id, workflow_id, runtime_job_id, artifact_type, data)
         VALUES ($1, $2, $3, $4, $5::jsonb)",
    )
    .bind("legacy-orphan-artifact")
    .bind("missing-legacy-workflow")
    .bind("missing-legacy-runtime-job")
    .bind("test")
    .bind("{}")
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn insert_runtime_job_with_status(
    store: &WorkflowRuntimeStore,
    id: &str,
    status: &str,
) -> anyhow::Result<()> {
    let command_id = test_runtime_job_command_id(store, id).await?;
    sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(id)
    .bind(command_id)
    .bind("codex_jsonrpc")
    .bind("codex-default")
    .bind(status)
    .bind("{}")
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn test_runtime_job_command_id(
    store: &WorkflowRuntimeStore,
    id: &str,
) -> anyhow::Result<String> {
    let workflow = issue_instance("implementing").with_id(format!("status-test-workflow-{id}"));
    store.upsert_instance(&workflow).await?;
    let command = WorkflowCommand::enqueue_activity("status_test", format!("status-test-{id}"));
    store.enqueue_command(&workflow.id, None, &command).await
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
