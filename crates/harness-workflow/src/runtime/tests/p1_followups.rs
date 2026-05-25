use super::*;

#[tokio::test]
async fn runtime_store_get_instance_by_pr_prefers_issue_bound_workflow() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let issue_bound = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        WorkflowSubject::new("issue", "issue:77"),
    )
    .with_id("project-a::owner/repo::issue:77")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/repo",
        "issue_number": 77,
        "pr_number": 880,
    }));
    let pr_only = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        WorkflowSubject::new("pull_request", "pr:880"),
    )
    .with_id("project-a::owner/repo::pr:880")
    .with_data(json!({
        "project_id": "project-a",
        "repo": "owner/repo",
        "pr_number": 880,
    }));
    store.upsert_instance(&issue_bound).await?;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    store.upsert_instance(&pr_only).await?;

    let found = store
        .get_instance_by_pr("github_issue_pr", "project-a", Some("owner/repo"), 880)
        .await?
        .expect("matching runtime workflow should be found");
    assert_eq!(found.id, issue_bound.id);
    Ok(())
}

#[tokio::test]
async fn runtime_worker_completes_job_when_workflow_already_done() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("done");
    store.upsert_instance(&instance).await?;
    let job = store
        .enqueue_runtime_job(
            "command-terminal-done",
            RuntimeKind::ClaudeCode,
            "claude-default",
            json!({ "activity": "implement_issue", "workflow_id": instance.id }),
        )
        .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let worker = RuntimeWorker::new(&store, "runtime-1");
    let executor = CountingRuntimeExecutor {
        result: ActivityResult::failed("implement_issue", "should not run", "unexpected call"),
        calls: calls.clone(),
    };

    let completed = worker
        .run_once(&executor)
        .await?
        .expect("worker should complete stale terminal job");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(completed.id, job.id);
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    let output: ActivityResult =
        serde_json::from_value(completed.output.expect("activity result output"))?;
    assert_eq!(output.status, ActivityStatus::Succeeded);
    assert!(output.summary.contains("already terminal (done)"));
    Ok(())
}
