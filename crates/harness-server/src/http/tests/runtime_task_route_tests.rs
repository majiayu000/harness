use super::*;

#[tokio::test]
async fn list_tasks_marks_runtime_submission_summaries_degraded_when_store_failed(
) -> anyhow::Result<()> {
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
    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .with_state(state.clone());

    let response = app
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["data"].as_array().map(Vec::len), Some(0));
    assert_eq!(body["degraded"]["partial"], true);
    assert_eq!(
        body["degraded"]["missing"],
        serde_json::json!(["workflow_runtime_submissions"])
    );
    assert_eq!(
        body["degraded"]["reason"],
        "runtime_submission_summaries_unavailable"
    );
    Ok(())
}

#[tokio::test]
async fn list_tasks_includes_runtime_issue_submissions() -> anyhow::Result<()> {
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
    let app = Router::new()
        .route("/tasks", get(list_tasks).post(task_routes::create_task))
        .with_state(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "repo": "owner/repo",
        "issue": 52,
        "labels": ["bug"]
    });
    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let created = response_json(create_response).await?;
    let task_id = created["task_id"]
        .as_str()
        .expect("runtime submission should return a task handle")
        .to_string();
    assert_eq!(state.core.tasks.count(), before_count);

    let list_response = app
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed = response_json(list_response).await?;
    let tasks = listed["data"].as_array().expect("tasks should be an array");
    let runtime_task = tasks
        .iter()
        .find(|task| task["id"] == task_id)
        .expect("runtime issue submission should be listed");
    let canonical_project_root = project_root.canonicalize()?;

    assert_eq!(runtime_task["task_kind"], "issue");
    assert_eq!(runtime_task["status"], "waiting");
    assert_eq!(runtime_task["external_id"], "issue:52");
    assert_eq!(runtime_task["repo"], "owner/repo");
    assert_eq!(runtime_task["description"], "issue #52");
    assert_eq!(
        runtime_task["project"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert_eq!(runtime_task["scheduler"]["authority_state"], "queued");
    assert!(runtime_task["workflow"]["id"]
        .as_str()
        .is_some_and(|id| id.ends_with("::repo:owner/repo::issue:52")));
    assert_eq!(runtime_task["workflow"]["definition_id"], "github_issue_pr");
    assert_eq!(runtime_task["workflow"]["state"], "planning");
    assert_eq!(runtime_task["workflow"]["issue_number"], 52);
    Ok(())
}

#[tokio::test]
async fn merge_task_accepts_runtime_workflow_task_handle() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:53"),
    )
    .with_id("runtime-ready-53")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 53,
        "pr_number": 125,
        "pr_url": "https://github.com/owner/repo/pull/125",
        "task_id": "runtime-ready-task",
    }));
    store.upsert_instance(&workflow).await?;
    let app = Router::new()
        .route("/tasks/{id}/merge", post(task_mutation_routes::merge_task))
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/runtime-ready-task/merge")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "merge_approved");
    assert_eq!(body["execution_path"], "workflow_runtime");
    let updated = store
        .get_instance("runtime-ready-53")
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "done");
    assert_eq!(updated.data["last_decision"], "approve_merge");
    assert_eq!(updated.data["merge_approved_task_id"], "runtime-ready-task");
    let events = store.events_for("runtime-ready-53").await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "MergeApproved"));
    let decisions = store.decisions_for("runtime-ready-53").await?;
    assert!(decisions
        .iter()
        .any(|record| record.accepted && record.decision.decision == "approve_merge"));
    let commands = store.commands_for("runtime-ready-53").await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::MarkDone
    );
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_merge_endpoint_approves_ready_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:54"),
    )
    .with_id("runtime-ready-54")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 54,
        "pr_number": 126,
        "pr_url": "https://github.com/owner/repo/pull/126",
        "task_id": "runtime-ready-task-54",
    }));
    store.upsert_instance(&workflow).await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/merge",
            post(task_mutation_routes::merge_workflow_runtime),
        )
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/merge")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "workflow_id": "runtime-ready-54" }).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response_json(response).await?;
    assert_eq!(body["workflow_id"], "runtime-ready-54");
    let updated = store
        .get_instance("runtime-ready-54")
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "done");
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_cancel_endpoint_cancels_issue_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = harness_core::types::TaskId::from_str("runtime-cancel-task-55");
    let submission = crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 55,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
        },
    )
    .await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/cancel",
            post(task_mutation_routes::cancel_workflow_runtime),
        )
        .with_state(state.clone());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/cancel")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "workflow_id": submission.workflow_id }).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["execution_path"], "workflow_runtime");
    let updated = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "cancelled");
    assert_eq!(updated.data["last_decision"], "cancel_issue_submission");

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/cancel")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "workflow_id": submission.workflow_id }).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "workflow already terminal");
    assert_eq!(body["state"], "cancelled");
    Ok(())
}

#[tokio::test]
async fn list_tasks_includes_runtime_prompt_submissions() -> anyhow::Result<()> {
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
    let app = Router::new()
        .route("/tasks", get(list_tasks).post(task_routes::create_task))
        .with_state(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "fix listed runtime prompt",
        "source": "dashboard",
        "external_id": "manual:prompt:list"
    });
    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let created = response_json(create_response).await?;
    let task_id = created["task_id"]
        .as_str()
        .expect("runtime submission should return a task handle")
        .to_string();
    assert_eq!(state.core.tasks.count(), before_count);

    let list_response = app
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed = response_json(list_response).await?;
    let tasks = listed["data"].as_array().expect("tasks should be an array");
    let runtime_task = tasks
        .iter()
        .find(|task| task["id"] == task_id)
        .expect("runtime prompt submission should be listed");
    let canonical_project_root = project_root.canonicalize()?;

    assert_eq!(runtime_task["task_kind"], "prompt");
    assert_eq!(runtime_task["status"], "implementing");
    assert_eq!(runtime_task["source"], "dashboard");
    assert_eq!(runtime_task["external_id"], "manual:prompt:list");
    assert_eq!(runtime_task["description"], "prompt task");
    assert_eq!(
        runtime_task["project"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert_eq!(runtime_task["scheduler"]["authority_state"], "running");
    assert_eq!(runtime_task["workflow"]["id"], created["workflow_id"]);
    assert_eq!(runtime_task["workflow"]["definition_id"], "prompt_task");
    assert_eq!(runtime_task["workflow"]["state"], "implementing");
    Ok(())
}

#[tokio::test]
async fn list_tasks_paginates_past_runtime_only_second_page() -> anyhow::Result<()> {
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
    let app = Router::new()
        .route("/tasks", get(list_tasks).post(task_routes::create_task))
        .with_state(state);

    let mut created_ids = Vec::new();
    for index in 0..3 {
        let body = serde_json::json!({
            "project": project_root.display().to_string(),
            "prompt": format!("runtime prompt page {index}"),
            "source": "dashboard",
            "external_id": format!("manual:prompt:page:{index}")
        });
        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))?,
            )
            .await?;
        assert_eq!(create_response.status(), StatusCode::ACCEPTED);
        let created = response_json(create_response).await?;
        created_ids.push(
            created["task_id"]
                .as_str()
                .expect("runtime submission should return a task handle")
                .to_string(),
        );
    }

    let mut cursor: Option<String> = None;
    let mut listed_ids = Vec::new();
    for page_index in 0..3 {
        let uri = match cursor.as_deref() {
            Some(cursor) => format!(
                "/tasks?limit=1&cursor={}",
                cursor
                    .replace('+', "%2B")
                    .replace(':', "%3A")
                    .replace('|', "%7C")
            ),
            None => "/tasks?limit=1".to_string(),
        };
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty())?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await?;
        let tasks = body["data"].as_array().expect("tasks should be an array");
        assert_eq!(tasks.len(), 1, "page {page_index} should contain one task");
        listed_ids.push(
            tasks[0]["id"]
                .as_str()
                .expect("task id should be present")
                .to_string(),
        );
        cursor = body["page"]["next_cursor"].as_str().map(str::to_string);
        if page_index < 2 {
            assert!(
                body["page"]["has_more"].as_bool().unwrap_or(false),
                "page {page_index} should expose another runtime-only page"
            );
            assert!(cursor.is_some(), "page {page_index} should return a cursor");
        }
    }

    created_ids.sort();
    listed_ids.sort();
    assert_eq!(listed_ids, created_ids);
    Ok(())
}

#[tokio::test]
async fn create_task_with_blocked_issue_returns_runtime_state() -> anyhow::Result<()> {
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
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "repo": "owner/repo",
        "issue": 43,
        "depends_on": ["missing-dependency"]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let resp = response_json(response).await?;
    assert!(resp["task_id"].is_string());
    assert_eq!(resp["status"], "awaiting_dependencies");
    assert_eq!(resp["execution_path"], "workflow_runtime");

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        43,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.state, "awaiting_dependencies");
    assert_eq!(
        instance.data["depends_on"],
        serde_json::json!(["missing-dependency"])
    );
    Ok(())
}

#[tokio::test]
async fn create_task_empty_request_returns_bad_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let app = task_app(state);

    let body = serde_json::json!({});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}
