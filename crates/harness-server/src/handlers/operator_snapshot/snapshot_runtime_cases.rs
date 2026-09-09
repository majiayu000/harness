#[tokio::test]
async fn includes_runtime_only_failed_workflow_in_recent_failures() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-fail-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = runtime_workflow(
        "failed",
        "issue:runtime-fail",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-fail",
            "submission_id": "runtime-only-fail",
            "failure_reason": "runtime workflow failed without TaskStore row",
        }),
    )
    .with_id("runtime-only-fail-workflow".to_string());
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let failures = body["recent_failures"]
        .as_array()
        .expect("recent_failures array");
    assert!(
        failures.iter().any(|row| {
            row["task_id"] == "runtime-only-fail"
                && row["error"] == "runtime workflow failed without TaskStore row"
        }),
        "expected runtime-only failure in recent_failures: {failures:?}"
    );
    Ok(())
}

#[tokio::test]
async fn includes_runtime_only_stalled_workflow_in_stalled_tasks() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-stall-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = runtime_workflow(
        "implementing",
        "issue:runtime-stall",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-stall",
            "submission_id": "runtime-only-stall",
        }),
    )
    .with_id("runtime-only-stall-workflow".to_string());
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    age_workflow(store, "runtime-only-stall-workflow").await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let stalled = body["retry"]["stalled_tasks"]
        .as_array()
        .expect("stalled_tasks array");
    assert!(
        stalled.iter().any(|row| {
            row["task_id"] == "runtime-only-stall" && row["status"] == "implementing"
        }),
        "expected runtime-only stalled workflow in stalled_tasks: {stalled:?}"
    );
    Ok(())
}

#[tokio::test]
async fn does_not_double_count_workflow_backed_by_task_row() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-dedupe-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let mut task = crate::task_runner::TaskState {
        id: harness_core::types::TaskId("shared-failure-task".to_string()),
        task_kind: crate::task_runner::TaskKind::Issue,
        status: crate::task_runner::TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::Task),
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: Some("legacy task failure".to_string()),
        source: None,
        external_id: Some("issue:shared-failure".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(std::path::PathBuf::from("/tmp/harness-shared")),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: Some(Utc::now().to_rfc3339()),
        priority: 0,
        phase: crate::task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
        version: 0,
    };
    task.status = crate::task_runner::TaskStatus::Failed;
    state.core.tasks.insert(&task).await;

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = runtime_workflow(
        "failed",
        "issue:shared-failure",
        serde_json::json!({
            "project_id": "/tmp/harness-shared",
            "task_id": "shared-failure-task",
            "submission_id": "shared-failure-task",
            "failure_reason": "duplicate runtime view of the same failure",
        }),
    )
    .with_id("shared-failure-workflow".to_string());
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let failures = body["recent_failures"]
        .as_array()
        .expect("recent_failures array");
    let shared = failures
        .iter()
        .filter(|row| row["task_id"] == "shared-failure-task")
        .count();
    assert_eq!(
        shared, 1,
        "expected exactly one shared failure row, got {failures:?}"
    );
    assert_eq!(
        failures
            .iter()
            .find(|row| row["task_id"] == "shared-failure-task")
            .and_then(|row| row["error"].as_str()),
        Some("legacy task failure"),
        "TaskStore row should win when both stores expose the same handle"
    );
    Ok(())
}

#[tokio::test]
async fn excludes_queued_and_dependency_blocked_runtime_workflows_from_stalled(
) -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-queue-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");

    let queued = runtime_workflow(
        "scheduled",
        "issue:runtime-queued",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-queued",
            "submission_id": "runtime-queued",
        }),
    )
    .with_id("runtime-queued-workflow".to_string());
    let awaiting = runtime_workflow(
        "awaiting_dependencies",
        "issue:runtime-awaiting",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-awaiting",
            "submission_id": "runtime-awaiting",
        }),
    )
    .with_id("runtime-awaiting-workflow".to_string());
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &queued).await?;
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &awaiting).await?;
    age_workflow(store, "runtime-queued-workflow").await?;
    age_workflow(store, "runtime-awaiting-workflow").await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let stalled = body["retry"]["stalled_tasks"]
        .as_array()
        .expect("stalled_tasks array");
    assert!(
        stalled.iter().all(|row| {
            row["task_id"] != "runtime-queued" && row["task_id"] != "runtime-awaiting"
        }),
        "queued/awaiting runtime workflows must not appear as stalled: {stalled:?}"
    );
    Ok(())
}

#[tokio::test]
async fn collapses_child_workflow_failure_into_parent_row() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-child-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");

    let parent = runtime_workflow(
        "failed",
        "issue:runtime-parent-fail",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-parent-fail",
            "submission_id": "runtime-parent-fail",
            "failure_reason": "parent failed after child quality gate",
        }),
    )
    .with_id("runtime-parent-fail-workflow".to_string());
    let child = runtime_workflow(
        "failed",
        "issue:runtime-child-fail",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-parent-fail",
            "failure_reason": "child quality gate failed",
        }),
    )
    .with_id("runtime-child-fail-workflow".to_string())
    .with_parent("runtime-parent-fail-workflow");
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &parent).await?;
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &child).await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let failures = body["recent_failures"]
        .as_array()
        .expect("recent_failures array");
    assert!(
        failures
            .iter()
            .any(|row| row["task_id"] == "runtime-parent-fail"),
        "expected parent failure row: {failures:?}"
    );
    assert!(
        failures
            .iter()
            .all(|row| row["task_id"] != "runtime-child-fail-workflow"
                && row["task_id"] != "runtime-child-fail"),
        "child failure must not consume a recent_failures slot: {failures:?}"
    );
    Ok(())
}

#[tokio::test]
async fn recent_failures_keep_older_root_when_newer_children_fill_limit() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-root-limit-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");

    let root = runtime_workflow(
        "failed",
        "issue:runtime-root-limit",
        serde_json::json!({
            "project_id": "/tmp/harness-runtime-root-limit",
            "submission_id": "runtime-root-limit",
            "failure_reason": "older root failure",
        }),
    )
    .with_id("runtime-root-limit-workflow".to_string());
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &root).await?;
    age_workflow(store, "runtime-root-limit-workflow").await?;

    for index in 0..MAX_TASKS {
        let child = runtime_workflow(
            "failed",
            &format!("issue:runtime-child-limit-{index}"),
            serde_json::json!({
                "failure_reason": "newer same-definition child failure",
            }),
        )
        .with_id(format!("runtime-child-limit-{index}"))
        .with_parent("runtime-root-limit-workflow");
        test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &child).await?;
    }

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let failures = body["recent_failures"]
        .as_array()
        .expect("recent_failures array");
    assert!(
        failures
            .iter()
            .any(|row| row["task_id"] == "runtime-root-limit"),
        "older root failure must survive the per-definition limit: {failures:?}"
    );
    Ok(())
}

#[tokio::test]
async fn recent_failures_include_non_propagating_pr_feedback_child() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-pr-feedback-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");

    let parent = runtime_workflow(
        "awaiting_feedback",
        "issue:pr-feedback-parent",
        serde_json::json!({
            "project_id": "/tmp/harness-pr-feedback-parent",
            "submission_id": "pr-feedback-parent",
            "pr_number": 77,
        }),
    )
    .with_id("pr-feedback-parent-workflow".to_string());
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-child-failed".to_string())
    .with_parent("pr-feedback-parent-workflow")
    .with_server_data(serde_json::json!({
        "failure_reason": "PR feedback inspection failed permanently.",
    }));
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &parent).await?;
    test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &child).await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let failures = body["recent_failures"]
        .as_array()
        .expect("recent_failures array");
    assert!(
        failures.iter().any(|row| {
            row["task_id"] == "pr-feedback-child-failed"
                && row["error"] == "PR feedback inspection failed permanently."
        }),
        "non-propagating PR-feedback child failure must stay visible: {failures:?}"
    );
    assert!(
        failures
            .iter()
            .all(|row| row["task_id"] != "pr-feedback-parent"),
        "awaiting_feedback parent must not appear as a recent failure: {failures:?}"
    );
    Ok(())
}
