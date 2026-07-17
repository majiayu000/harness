fn runtime_job_status_str(status: RuntimeJobStatus) -> anyhow::Result<String> {
    let value = serde_json::to_value(status)?;
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("runtime job status did not serialize to a string"))
}

async fn insert_legacy_orphan_command(
    store: &WorkflowRuntimeStore,
    command_id: &str,
    workflow_id: &str,
    command: &WorkflowCommand,
    status: WorkflowCommandStatus,
) -> anyhow::Result<()> {
    let data = serde_json::to_string(command)?;
    sqlx::query(
        "INSERT INTO workflow_commands
            (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
    )
    .bind(command_id)
    .bind(workflow_id)
    .bind(Option::<String>::None)
    .bind("enqueue_activity")
    .bind(&command.dedupe_key)
    .bind(status.as_str())
    .bind(&data)
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn insert_runtime_job_record(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
) -> anyhow::Result<()> {
    let data = serde_json::to_string(job)?;
    sqlx::query(
        "INSERT INTO runtime_jobs
            (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
    )
    .bind(&job.id)
    .bind(&job.command_id)
    .bind("codex_jsonrpc")
    .bind(&job.runtime_profile)
    .bind(runtime_job_status_str(job.status)?)
    .bind(job.not_before)
    .bind(&data)
    .execute(store.pool())
    .await?;
    Ok(())
}

async fn update_runtime_job_record(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
) -> anyhow::Result<()> {
    let data = serde_json::to_string(job)?;
    sqlx::query(
        "UPDATE runtime_jobs
         SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
         WHERE id = $4",
    )
    .bind(runtime_job_status_str(job.status)?)
    .bind(job.not_before)
    .bind(&data)
    .bind(&job.id)
    .execute(store.pool())
    .await?;
    Ok(())
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
