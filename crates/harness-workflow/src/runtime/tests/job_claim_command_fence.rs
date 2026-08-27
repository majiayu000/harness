#[tokio::test]
async fn runtime_job_claim_requires_a_live_dispatched_command() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("implementing").with_id("job-claim-command-fence");
    store.force_upsert_lifecycle_state_for_test(&instance).await?;

    let failed_command = WorkflowCommand::enqueue_activity("implement_issue", "failed-command");
    let failed_id = store
        .enqueue_command(&instance.id, None, &failed_command)
        .await?;
    store
        .enqueue_runtime_job(
            &failed_id,
            RuntimeKind::CodexExec,
            "default",
            failed_command.command,
        )
        .await?;
    assert_eq!(
        store
            .get_command(&failed_id)
            .await?
            .expect("command should remain readable")
            .status,
        WorkflowCommandStatus::Dispatched
    );
    store
        .mark_command_status(&failed_id, WorkflowCommandStatus::Failed)
        .await?;

    let stale_command = WorkflowCommand::enqueue_activity("implement_issue", "stale-command");
    let stale_id = store
        .enqueue_command(&instance.id, None, &stale_command)
        .await?;
    store
        .enqueue_runtime_job(
            &stale_id,
            RuntimeKind::CodexExec,
            "default",
            stale_command.command,
        )
        .await?;
    let replacement_id = store
        .enqueue_command(
            &instance.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "replacement-command"),
        )
        .await?;
    sqlx::query(
        "UPDATE workflow_commands
         SET status = 'superseded', superseded_by_command_id = $2
         WHERE id = $1",
    )
    .bind(&stale_id)
    .bind(&replacement_id)
    .execute(store.pool())
    .await?;

    assert!(store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .is_none());
    Ok(())
}
