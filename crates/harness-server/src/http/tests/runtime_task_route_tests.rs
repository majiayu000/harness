use super::*;
use harness_workflow::issue_lifecycle::IssueLifecycleState;

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
        .clone()
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
    assert_eq!(runtime_task["status"], "planning");
    assert_eq!(runtime_task["phase"], "plan");
    assert_eq!(runtime_task["external_id"], "issue:52");
    assert_eq!(runtime_task["repo"], "owner/repo");
    assert_eq!(runtime_task["description"], "issue #52");
    assert_eq!(
        runtime_task["project"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert_eq!(runtime_task["scheduler"]["authority_state"], "running");
    assert!(runtime_task["workflow"]["id"]
        .as_str()
        .is_some_and(|id| id.ends_with("::repo:owner/repo::issue:52")));
    assert_eq!(runtime_task["workflow"]["definition_id"], "github_issue_pr");
    assert_eq!(runtime_task["workflow"]["state"], "planning");
    assert_eq!(runtime_task["workflow"]["issue_number"], 52);
    assert!(runtime_task
        .get("source")
        .is_none_or(serde_json::Value::is_null));

    let planning_response = app
        .oneshot(
            Request::builder()
                .uri("/tasks?status=planning")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(planning_response.status(), StatusCode::OK);
    let planning_listed = response_json(planning_response).await?;
    let planning_tasks = planning_listed["data"]
        .as_array()
        .expect("planning-filtered tasks should be an array");
    assert!(
        planning_tasks.iter().any(|task| task["id"] == task_id),
        "status=planning should include runtime-backed planning workflows"
    );
    Ok(())
}

#[tokio::test]
async fn get_task_runtime_issue_exposes_tracker_identity() -> anyhow::Result<()> {
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
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = harness_core::types::TaskId::from_str("runtime-github-issue-detail");

    crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 64,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:64"),
            remote_fact_hash: None,
        },
    )
    .await?;
    let app = Router::new()
        .route("/tasks/{id}", get(get_task))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/runtime-github-issue-detail")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["source"], "github");
    assert_eq!(body["external_id"], "issue:64");
    assert_eq!(body["tracker_source"], "github");
    assert_eq!(body["tracker_external_id"], "issue:64");
    Ok(())
}

#[tokio::test]
async fn get_task_runtime_issue_projects_detail_status_from_shared_projection() -> anyhow::Result<()>
{
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
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let cases = [
        (
            "detail-active-workflow",
            "detail-active-task",
            "implementing",
            "implementing",
            "implement",
            "running",
            None,
        ),
        (
            "detail-review-wait-workflow",
            "detail-review-wait-task",
            "awaiting_feedback",
            "waiting",
            "review",
            "queued",
            None,
        ),
        (
            "detail-terminal-workflow",
            "detail-terminal-task",
            "failed",
            "failed",
            "terminal",
            "failed",
            Some("task"),
        ),
    ];

    for &(workflow_id, task_id, workflow_state, _, _, _, _) in &cases {
        let workflow = harness_workflow::runtime::WorkflowInstance::new(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            workflow_state,
            harness_workflow::runtime::WorkflowSubject::new("issue", workflow_id),
        )
        .with_id(workflow_id)
        .with_data(serde_json::json!({
            "project_id": project_root.to_string_lossy(),
            "repo": "owner/repo",
            "issue_number": 70,
            "submission_id": task_id,
            "task_id": format!("{task_id}-legacy"),
        }));
        store.upsert_instance(&workflow).await?;
    }

    let app = Router::new()
        .route("/tasks/{id}", get(get_task))
        .with_state(state);

    for &(_, task_id, workflow_state, status, phase, scheduler_state, failure_kind) in &cases {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/tasks/{task_id}"))
                    .body(Body::empty())?,
            )
            .await?;

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await?;
        assert_eq!(body["id"], task_id);
        assert_eq!(body["task_id"], task_id);
        assert_eq!(body["submission_id"], task_id);
        assert_eq!(body["workflow_state"], workflow_state);
        assert_eq!(body["workflow"]["state"], workflow_state);
        assert_eq!(body["status"], status);
        assert_eq!(body["phase"], phase);
        assert_eq!(body["scheduler"]["authority_state"], scheduler_state);
        match failure_kind {
            Some(kind) => assert_eq!(body["failure_kind"], *kind),
            None => assert!(body
                .get("failure_kind")
                .is_none_or(serde_json::Value::is_null)),
        }
    }

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
        "merge_method": "rebase",
        "merge_delete_branch": false,
        "merge_require_review_threads_resolved": false,
        "merge_require_clean_merge_state": true,
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
    assert_eq!(updated.state, "merging");
    assert_eq!(updated.data["last_decision"], "approve_merge");
    assert_eq!(updated.data["merge_approved_task_id"], "runtime-ready-task");
    assert_eq!(updated.data["merge_delete_branch"], false);
    assert_eq!(updated.data["merge_require_review_threads_resolved"], false);
    assert_eq!(updated.data["merge_require_clean_merge_state"], true);
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
        harness_workflow::runtime::WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(commands[0].command.activity_name(), Some("merge_pr"));
    assert_eq!(commands[0].command.command["merge_method"], "rebase");
    assert_eq!(commands[0].command.command["delete_branch"], false);
    assert_eq!(
        commands[0].command.command["require_review_threads_resolved"],
        false
    );
    assert_eq!(
        commands[0].command.command["require_clean_merge_state"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn merge_task_reports_legacy_workflow_actual_state_when_not_ready() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_issue_workflows(dir.path()).await?;
    let task_id = task_runner::TaskId::from_str("legacy-done-task-56");
    let project_id = dir.path().to_string_lossy().into_owned();
    let repo = "owner/repo";
    let pr_number = 127;
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.task_kind = task_runner::TaskKind::Issue;
    task.pr_url = Some(format!("https://github.com/{repo}/pull/{pr_number}"));
    task.project_root = Some(dir.path().to_path_buf());
    task.repo = Some(repo.to_string());
    state.core.tasks.insert(&task).await;

    let workflows = state
        .core
        .issue_workflow_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("issue workflow store should be configured"))?;
    workflows
        .record_issue_scheduled(&project_id, Some(repo), 56, task_id.as_str(), &[], false)
        .await?;
    workflows
        .record_pr_detected(
            &project_id,
            Some(repo),
            56,
            task_id.as_str(),
            pr_number,
            &format!("https://github.com/{repo}/pull/{pr_number}"),
        )
        .await?;
    workflows
        .record_terminal_for_issue(&project_id, Some(repo), 56, IssueLifecycleState::Done, None)
        .await?;
    let app = Router::new()
        .route("/tasks/{id}/merge", post(task_mutation_routes::merge_task))
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/tasks/{}/merge", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "workflow not in ready_to_merge state");
    assert_eq!(body["state"], "done");
    let workflow = workflows
        .get_by_pr(&project_id, Some(repo), pr_number)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow should still exist"))?;
    assert_eq!(workflow.state, IssueLifecycleState::Done);
    Ok(())
}

#[tokio::test]
async fn merge_task_approves_legacy_ready_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_issue_workflows(dir.path()).await?;
    let task_id = task_runner::TaskId::from_str("legacy-ready-task-57");
    let project_id = dir.path().to_string_lossy().into_owned();
    let repo = "owner/repo";
    let pr_number = 128;
    let pr_url = format!("https://github.com/{repo}/pull/{pr_number}");
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.task_kind = task_runner::TaskKind::Issue;
    task.pr_url = Some(pr_url.clone());
    task.project_root = Some(dir.path().to_path_buf());
    task.repo = Some(repo.to_string());
    state.core.tasks.insert(&task).await;

    let workflows = state
        .core
        .issue_workflow_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("issue workflow store should be configured"))?;
    workflows
        .record_issue_scheduled(&project_id, Some(repo), 57, task_id.as_str(), &[], false)
        .await?;
    workflows
        .record_pr_detected(
            &project_id,
            Some(repo),
            57,
            task_id.as_str(),
            pr_number,
            &pr_url,
        )
        .await?;
    workflows
        .record_terminal_for_pr(&project_id, Some(repo), pr_number, true, false, None)
        .await?;
    let app = Router::new()
        .route("/tasks/{id}/merge", post(task_mutation_routes::merge_task))
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/tasks/{}/merge", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "merge_approved");
    let workflow = workflows
        .get_by_pr(&project_id, Some(repo), pr_number)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow should still exist"))?;
    assert_eq!(workflow.state, IssueLifecycleState::Done);
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
    assert_eq!(updated.state, "merging");
    let commands = store.commands_for("runtime-ready-54").await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].command.activity_name(), Some("merge_pr"));
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
            remote_fact_hash: None,
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
async fn cancel_task_accepts_runtime_submission_handle_without_task_row() -> anyhow::Result<()> {
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
    let task_id = harness_core::types::TaskId::from_str("runtime-compat-cancel-task-58");
    let submission = crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 58,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
            remote_fact_hash: None,
        },
    )
    .await?;
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none(),
        "runtime submissions should not require legacy task rows"
    );
    let app = Router::new()
        .route("/tasks/{id}/cancel", post(task_routes::cancel_task))
        .with_state(state.clone());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/tasks/{}/cancel", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["execution_path"], "workflow_runtime");
    assert_eq!(body["workflow_id"], submission.workflow_id);
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none(),
        "legacy compatibility cancel must not create a TaskStore row"
    );
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
                .uri(format!("/tasks/{}/cancel", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "task already in terminal state");
    Ok(())
}

#[tokio::test]
async fn get_task_runtime_issue_surfaces_failure_reason() -> anyhow::Result<()> {
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
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "failed",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1299"),
    )
    .with_id("issue-1299")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 1299,
        "task_id": "runtime-task-1299",
        "task_ids": ["runtime-task-1299"],
        "failure_reason": "WorktreeCollision: workspace path is managed by another harness session",
    }));
    store.upsert_instance(&workflow).await?;
    let app = Router::new()
        .route("/tasks/{id}", get(get_task))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/runtime-task-1299")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "failed");
    assert_eq!(
        body["error"],
        "WorktreeCollision: workspace path is managed by another harness session"
    );
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
    assert_eq!(resp["status"], "awaiting_deps");
    assert_eq!(resp["workflow_state"], "awaiting_dependencies");
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
