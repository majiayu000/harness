use super::*;

#[tokio::test]
async fn get_task_returns_not_found_for_missing_id() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let app = runtime_submission_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions/nonexistent-id")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn get_task_returns_service_unavailable_when_required_workflow_runtime_store_missing(
) -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("workflow_runtime_store")
                .failed("failed to connect to Postgres"),
        ];
    state_mut.degraded_subsystems = vec!["workflow_runtime_store"];
    let app = runtime_submission_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions/runtime-only-task")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "workflow runtime store unavailable");
    assert_eq!(body["message"], "workflow runtime store is unavailable");
    Ok(())
}

#[tokio::test]
async fn get_task_proof_returns_service_unavailable_when_required_workflow_runtime_store_missing(
) -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("workflow_runtime_store")
                .failed("failed to connect to Postgres"),
        ];
    state_mut.degraded_subsystems = vec!["workflow_runtime_store"];
    let app = runtime_submission_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions/runtime-only-task/proof")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "workflow runtime store unavailable");
    assert_eq!(body["message"], "workflow runtime store is unavailable");
    Ok(())
}

#[tokio::test]
async fn create_then_get_task_returns_state() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;

    let create_body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "add tests",
    });
    let create_resp = runtime_submission_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/submissions")
                .header("content-type", "application/json")
                .body(Body::from(create_body.to_string()))?,
        )
        .await?;
    assert_eq!(create_resp.status(), StatusCode::ACCEPTED);

    let create_json = response_json(create_resp).await?;
    let task_id = create_json["task_id"]
        .as_str()
        .expect("task_id should be string");
    assert_eq!(create_json["status"], "implementing");
    assert_eq!(create_json["execution_path"], "workflow_runtime");

    let get_resp = runtime_submission_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/workflows/runtime/submissions/{task_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_resp.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let get_body = get_resp.into_body().collect().await?.to_bytes();
    let task_json: serde_json::Value = serde_json::from_slice(&get_body)?;
    assert_eq!(task_json["id"], task_id);
    assert_eq!(task_json["status"], "implementing");
    assert_eq!(task_json["execution_path"], "workflow_runtime");
    assert_eq!(task_json["workflow"]["definition_id"], "prompt_task");
    Ok(())
}

#[tokio::test]
async fn get_task_hides_internal_system_input_metadata() -> anyhow::Result<()> {
    use axum::response::IntoResponse;

    let dir = tempfile::tempdir()?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Review,
        status: task_runner::TaskStatus::ReviewWaiting,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("periodic_review".to_string()),
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: None,
        repo: Some("owner/repo".to_string()),
        description: Some("periodic review".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Review,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,

        version: 0,
    };
    let task_id = task.id.to_string();

    let response = axum::Json(task).into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let task_json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(task_json["id"], task_id);
    assert!(task_json.get("system_input").is_none());
    Ok(())
}

#[tokio::test]
async fn runtime_issue_sse_stream_keeps_active_workflow_open_without_done() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;

    let create_response = runtime_submission_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/submissions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "repo": "owner/repo",
                        "issue": 44,
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let create_json = response_json(create_response).await?;
    let task_id = create_json["task_id"]
        .as_str()
        .expect("task id should be present");

    let response = Router::new()
        .route(
            "/api/workflows/runtime/submissions/{id}/stream",
            get(sse_routes::stream_runtime_submission_sse),
        )
        .with_state(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{task_id}/stream"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let mut body = response.into_body();
    let mut body_text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !body_text.contains("[workflow]") && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    body_text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => anyhow::bail!("active runtime workflow stream closed before replay"),
            Err(_) => break,
        }
    }
    assert!(body_text.contains("[workflow]"));
    assert!(!body_text.contains("\"type\":\"done\""));
    assert!(!body_text.contains("\"type\":\"Done\""));

    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(100), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    body_text.push_str(&String::from_utf8_lossy(&data));
                    assert!(!body_text.contains("\"type\":\"done\""));
                    assert!(!body_text.contains("\"type\":\"Done\""));
                }
            }
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => anyhow::bail!("active runtime workflow stream closed"),
            Err(_) => return Ok(()),
        }
    }

    anyhow::bail!("active runtime workflow stream did not become idle")
}
