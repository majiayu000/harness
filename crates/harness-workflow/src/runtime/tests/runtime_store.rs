use super::*;

#[tokio::test]
async fn dedupe_returns_latest_runtime_job_for_command() -> anyhow::Result<()> {
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

    let outcome = store
        .enqueue_runtime_job_for_pending_command(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"attempt": 3}),
            None,
        )
        .await?;

    assert_ne!(older.id, newer.id);
    match outcome {
        RuntimeJobEnqueueOutcome::AlreadyExists(job) => assert_eq!(job.id, newer.id),
        other => panic!("unexpected latest-job dedupe outcome: {other:?}"),
    }
    Ok(())
}
