use super::*;

#[tokio::test]
async fn get_task_returns_not_found_for_missing_id() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = task_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/nonexistent-id")
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
    let app = task_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/runtime-only-task")
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
    let app = task_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/runtime-only-task/proof")
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
    let create_resp = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
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

    let get_resp = task_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{task_id}"))
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
async fn create_tasks_batch_with_issues_returns_runtime_submissions() -> anyhow::Result<()> {
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
    let before_count = state.core.tasks.count();
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "issues": [42, 43],
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    for (entry, issue_number) in entries.iter().zip([42_u64, 43]) {
        assert_eq!(entry["status"], "implementing");
        assert_eq!(entry["execution_path"], "workflow_runtime");
        let workflow_id = assert_runtime_issue_submission(
            &state,
            &project_root,
            None,
            issue_number,
            entry["task_id"]
                .as_str()
                .expect("task id should be present"),
        )
        .await?;
        assert_eq!(entry["workflow_id"], workflow_id);
    }
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_prompts_returns_runtime_submissions() -> anyhow::Result<()> {
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
    let before_count = state.core.tasks.count();
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "tasks": [
                            { "description": "batch prompt 1" },
                            { "description": "batch prompt 2" }
                        ]
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    for (entry, prompt) in entries.iter().zip(["batch prompt 1", "batch prompt 2"]) {
        assert_eq!(entry["status"], "implementing");
        assert_eq!(entry["execution_path"], "workflow_runtime");
        let task_id = task_runner::TaskId::from_str(
            entry["task_id"]
                .as_str()
                .expect("task id should be present"),
        );
        assert!(state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none());
        let instance = store
            .get_instance_by_task_id(task_id.as_str())
            .await?
            .expect("runtime prompt submission should be persisted");
        assert_eq!(entry["workflow_id"], instance.id);
        assert_eq!(instance.definition_id, "prompt_task");
        assert_eq!(instance.state, "implementing");
        assert!(instance.data.get("prompt").is_none());
        assert_eq!(instance.data["prompt_summary"], "prompt task");
        assert_eq!(instance.data["prompt_chars"], prompt.chars().count());
        let prompt_ref = instance.data["prompt_ref"]
            .as_str()
            .expect("prompt ref should be persisted");
        assert_eq!(
            crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref)
                .as_deref(),
            Some(prompt)
        );
        let commands = store.commands_for(&instance.id).await?;
        assert_eq!(commands.len(), 1);
        assert!(commands[0].command.command.get("prompt").is_none());
        assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);
    }
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_conflicting_runtime_prompts_adds_dependencies(
) -> anyhow::Result<()> {
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
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "tasks": [
                            { "description": "update src/lib.rs first" },
                            { "description": "update src/lib.rs second" }
                        ]
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["serialized"], true);
    assert_eq!(entries[1]["serialized"], true);
    assert_eq!(
        entries[0]["conflict_files"],
        serde_json::json!(["src/lib.rs"])
    );
    assert_eq!(
        entries[1]["conflict_files"],
        serde_json::json!(["src/lib.rs"])
    );
    assert_eq!(entries[0]["status"], "implementing");
    assert!(
        entries[1]["status"] == "awaiting_dependencies" || entries[1]["status"] == "implementing"
    );

    let first_task_id = entries[0]["task_id"]
        .as_str()
        .expect("first task id should be present");
    let second_task_id = entries[1]["task_id"]
        .as_str()
        .expect("second task id should be present");
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let first = store
        .get_instance_by_task_id(first_task_id)
        .await?
        .expect("first runtime prompt submission should be persisted");
    let second = store
        .get_instance_by_task_id(second_task_id)
        .await?
        .expect("second runtime prompt submission should be persisted");

    assert_eq!(
        second.data["depends_on"],
        serde_json::json!([first_task_id])
    );
    assert_eq!(second.data["required_depends_on"], serde_json::json!([]));
    assert_eq!(
        second.data["serialization_depends_on"],
        serde_json::json!([first_task_id])
    );
    assert_eq!(store.commands_for(&first.id).await?.len(), 1);
    let second_commands = store.commands_for(&second.id).await?.len();
    match second.state.as_str() {
        "awaiting_dependencies" => assert_eq!(second_commands, 0),
        "implementing" => assert_eq!(second_commands, 1),
        state => panic!("unexpected second runtime prompt state {state}"),
    }
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_issues_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let before_count = state.core.tasks.count();
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "issues": [42, 43],
                        "repo": "owner/repo",
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert!(
            entry["error"].as_str().is_some_and(|error| error
                .contains("workflow runtime store is required for GitHub issue submissions")),
            "unexpected entry: {entry}"
        );
    }
    assert_eq!(state.core.tasks.count(), before_count);
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
async fn get_task_includes_round_telemetry_and_failure() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task_id = task_runner::TaskId::new();
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.rounds.push(task_runner::RoundResult::new(
        1,
        "implement",
        "upstream_failure",
        Some("provider error".to_string()),
        Some(TurnTelemetry {
            first_token_latency_ms: Some(123),
            completed_latency_ms: Some(456),
            ..Default::default()
        }),
        Some(TurnFailure {
            kind: TurnFailureKind::Upstream,
            provider: Some("anthropic-api".to_string()),
            upstream_status: Some(500),
            message: Some("API returned 500".to_string()),
            body_excerpt: Some("{\"type\":\"error\"}".to_string()),
        }),
    ));
    state.core.tasks.insert(&task).await;

    let response = task_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    let task_json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        task_json["rounds"][0]["telemetry"]["first_token_latency_ms"],
        123
    );
    assert_eq!(task_json["rounds"][0]["failure"]["kind"], "upstream");
    assert_eq!(task_json["rounds"][0]["failure"]["upstream_status"], 500);
    Ok(())
}

#[tokio::test]
async fn closed_task_sse_replay_includes_observability_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task_id = task_runner::TaskId::new();
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.rounds.push(task_runner::RoundResult::new(
        1,
        "review",
        "timeout",
        Some("reviewer stalled".to_string()),
        Some(TurnTelemetry {
            completed_latency_ms: Some(900),
            ..Default::default()
        }),
        Some(TurnFailure {
            kind: TurnFailureKind::Timeout,
            provider: Some("claude".to_string()),
            upstream_status: None,
            message: Some("timeout".to_string()),
            body_excerpt: None,
        }),
    ));
    state.core.tasks.insert(&task).await;

    let app = Router::new()
        .route("/tasks/{id}/stream", get(stream_task_sse))
        .with_state(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}/stream", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    let body_text = String::from_utf8(body.to_vec())?;
    let mut saw_telemetry = false;
    let mut saw_failure = false;
    let mut saw_timeout_failure = false;
    for line in body_text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let item: StreamItem = serde_json::from_str(data)?;
        if let StreamItem::MessageDelta { text } = item {
            saw_telemetry |= text.contains("telemetry=");
            if let Some((_, failure_json)) = text.split_once("\nfailure=") {
                saw_failure = true;
                let failure: TurnFailure = serde_json::from_str(failure_json.trim())?;
                saw_timeout_failure |= failure.kind == TurnFailureKind::Timeout;
            }
        }
    }
    assert!(saw_telemetry);
    assert!(saw_failure);
    assert!(saw_timeout_failure);

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

    let create_response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
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
        .route("/tasks/{id}/stream", get(stream_task_sse))
        .with_state(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{task_id}/stream"))
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
