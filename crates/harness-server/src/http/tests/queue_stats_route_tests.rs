use super::*;

#[tokio::test]
async fn queue_stats_matches_overview_for_runtime_active_work() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let project_id = dir.path().canonicalize()?.to_string_lossy().into_owned();
    let running = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "prompt:queue-stats-running"),
    )
    .with_id("queue-stats-running-workflow")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "task_id": "queue-stats-running-task",
    }));
    let queued = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "prompt:queue-stats-queued"),
    )
    .with_id("queue-stats-queued-workflow")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "task_id": "queue-stats-queued-task",
    }));
    store.upsert_instance(&running).await?;
    store.upsert_instance(&queued).await?;

    let app = Router::new()
        .route("/projects/queue-stats", get(project_queue_stats))
        .route("/api/overview", get(crate::handlers::overview::overview))
        .with_state(state);

    let queue_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/projects/queue-stats")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(queue_response.status(), StatusCode::OK);
    let queue_body = response_json(queue_response).await?;

    let overview_response = app
        .oneshot(
            Request::builder()
                .uri("/api/overview")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(overview_response.status(), StatusCode::OK);
    let overview_body = response_json(overview_response).await?;

    assert_eq!(
        queue_body["global"]["running"],
        overview_body["global"]["running"]
    );
    assert_eq!(
        queue_body["global"]["queued"],
        overview_body["global"]["queued"]
    );
    assert_eq!(queue_body["global"]["running"], serde_json::json!(1));
    assert_eq!(queue_body["global"]["queued"], serde_json::json!(1));
    assert!(queue_body["global"]["limit"].is_number());
    let project = &queue_body["projects"][project_id.as_str()];
    assert_eq!(project["running"], serde_json::json!(1));
    assert_eq!(project["queued"], serde_json::json!(1));
    assert!(project["limit"].is_number());

    Ok(())
}
