use super::*;
use harness_workflow::{
    issue_lifecycle::IssueLifecycleState,
    runtime::{
        RuntimeKind, WorkflowCommand, WorkflowCommandStatus, WorkflowCommandType, WorkflowInstance,
        WorkflowRuntimeStore, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
    },
};

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
            author_trust_class: None,
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
            author_trust_class: None,
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

#[rustfmt::skip]
#[tokio::test]
async fn workflow_runtime_recovery_endpoints_cover_contract() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await { return Ok(()); }
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state.core.workflow_runtime_store.as_ref().expect("workflow runtime store should be configured");
    let app = recovery_route_app(state.clone());

    for (route, state_name, status, issue_number) in [("/api/workflows/runtime/unblock", "blocked", "unblocked", 56), ("/api/workflows/runtime/retry", "failed", "retried", 57)] {
        let workflow_id = format!("runtime-{state_name}-{issue_number}");
        let mut data = serde_json::json!({"issue_number": issue_number});
        if state_name == "failed" { data["error_kind"] = serde_json::json!("timeout"); }
        let workflow = route_issue_workflow(&workflow_id, state_name, issue_number, data.clone());
        store.upsert_instance(&workflow).await?;
        let original = WorkflowCommand::new(WorkflowCommandType::EnqueueActivity, format!("{workflow_id}-original"), serde_json::json!({"activity": "implement_issue", "repo": "owner/repo", "issue_number": issue_number}));
        let runtime_job_id = enqueue_route_test_runtime_job(store, &workflow.id, &original).await?;
        data["last_stop"] = serde_json::json!({"state": state_name, "activity": "implement_issue", "runtime_job_id": runtime_job_id});
        if state_name == "failed" { data["last_stop"]["error_kind"] = serde_json::json!("timeout"); }
        store.upsert_instance(&workflow.with_data(data)).await?;
        let response = post_runtime_recovery(app.clone(), route, &workflow_id).await?;
        let actual = response.status(); let body = response_json(response).await?;
        assert_eq!(actual, StatusCode::OK); assert_eq!(body["status"], status); assert_eq!(body["state"], "implementing"); assert_eq!(store.get_instance(&workflow_id).await?.unwrap().state, "implementing");
        let commands = store.commands_for(&workflow_id).await?;
        let replayed = commands.iter().find(|command| command.status == WorkflowCommandStatus::Pending).expect("replayed recovery command should be pending");
        assert_eq!(replayed.command.command, original.command);
    }

    for (route, workflow_id, state_name, issue_number, last_stop) in [("/api/workflows/runtime/unblock", "runtime-blocked-partial-empty", "blocked", 61, serde_json::json!({})), ("/api/workflows/runtime/unblock", "runtime-blocked-partial-event", "blocked", 62, serde_json::json!({"event_id": 123})), ("/api/workflows/runtime/unblock", "runtime-blocked-partial-null", "blocked", 63, serde_json::json!({"state": null, "activity": null, "runtime_job_id": null, "error_kind": null})), ("/api/workflows/runtime/retry", "runtime-failed-partial-empty", "failed", 64, serde_json::json!({})), ("/api/workflows/runtime/retry", "runtime-failed-partial-event", "failed", 65, serde_json::json!({"event_id": 123})), ("/api/workflows/runtime/retry", "runtime-failed-partial-null", "failed", 66, serde_json::json!({"state": null, "activity": null, "runtime_job_id": null, "error_kind": null}))] {
        let data = serde_json::json!({"issue_number": issue_number, "last_stop": last_stop}); let workflow = route_issue_workflow(workflow_id, state_name, issue_number, data.clone()); store.upsert_instance(&workflow).await?; let response = post_runtime_recovery(app.clone(), route, workflow_id).await?; let actual = response.status(); let body = response_json(response).await?;
        assert_eq!(actual, StatusCode::CONFLICT); assert_eq!(body["error"], "workflow runtime recovery cannot determine a supported stopped activity"); assert_eq!(body["last_stop_activity"], serde_json::Value::Null); let stored = store.get_instance(workflow_id).await?.unwrap(); assert_eq!(stored.state, state_name); assert_eq!(stored.data, data); assert!(store.commands_for(workflow_id).await?.is_empty());
    }

    for workflow in [route_issue_workflow("runtime-blocked-58", "blocked", 58, serde_json::json!({})), route_issue_workflow("runtime-failed-59", "failed", 59, serde_json::json!({"error_kind": "configuration"}))] {
        store.upsert_instance(&workflow).await?;
    }
    for (route, workflow_id, code, error, field, value) in [
        ("/api/workflows/runtime/retry", "runtime-missing-60", StatusCode::NOT_FOUND, "workflow not found", "error", "workflow not found"),
        ("/api/workflows/runtime/retry", "runtime-blocked-58", StatusCode::CONFLICT, "workflow not in failed state", "state", "blocked"),
        ("/api/workflows/runtime/retry", "runtime-failed-59", StatusCode::CONFLICT, "workflow failure is not retryable", "error_kind", "configuration"),
    ] {
        let response = post_runtime_recovery(app.clone(), route, workflow_id).await?;
        let actual = response.status(); let body = response_json(response).await?;
        assert_eq!(actual, code); assert_eq!(body["error"], error); assert_eq!(body[field], value);
    }

    let unavailable = make_read_only_route_test_state(dir.path()).await?;
    let response = post_runtime_recovery(recovery_route_app(unavailable), "/api/workflows/runtime/retry", "runtime-blocked-58").await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response_json(response).await?["error"], "workflow runtime store unavailable");
    Ok(())
}

#[rustfmt::skip]
fn recovery_route_app(state: Arc<AppState>) -> Router {
    Router::new().route("/api/workflows/runtime/unblock", post(task_mutation_routes::unblock_workflow_runtime)).route("/api/workflows/runtime/retry", post(task_mutation_routes::retry_workflow_runtime)).with_state(state)
}

#[rustfmt::skip]
fn route_issue_workflow(workflow_id: &str, state: &str, issue_number: u64, data: serde_json::Value) -> WorkflowInstance {
    WorkflowInstance::new(GITHUB_ISSUE_PR_DEFINITION_ID, 1, state, WorkflowSubject::new("issue", format!("issue:{issue_number}"))).with_id(workflow_id).with_data(data)
}

#[rustfmt::skip]
async fn post_runtime_recovery(app: Router, route: &str, workflow_id: &str) -> anyhow::Result<axum::response::Response> {
    Ok(app.oneshot(Request::builder().method("POST").uri(route).header("content-type", "application/json").body(Body::from(serde_json::json!({"workflow_id": workflow_id, "reason": "operator supplied input"}).to_string()))?).await?)
}

#[rustfmt::skip]
async fn enqueue_route_test_runtime_job(store: &WorkflowRuntimeStore, workflow_id: &str, command: &WorkflowCommand) -> anyhow::Result<String> {
    let command_id = store.enqueue_command(workflow_id, None, command).await?;
    match store.enqueue_runtime_job_for_pending_command(&command_id, RuntimeKind::CodexJsonrpc, "codex-default", command.command.clone(), None).await? {
        harness_workflow::runtime::store::RuntimeJobEnqueueOutcome::Enqueued(job) => Ok(job.id),
        other => anyhow::bail!("expected route test runtime job, got {other:?}"),
    }
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
            author_trust_class: None,
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
async fn status_stalled_terminal_list_and_detail_surface_terminal_reason() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let task_id = task_runner::TaskId::from_str("stalled-budget-task");
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.status = task_runner::TaskStatus::Failed;
    task.phase = task_runner::TaskPhase::Terminal;
    task.turn = 2;
    task.error = Some(
        task_runner::TaskTerminalFailure::round_budget_exhausted(
            1,
            task_runner::TaskStatus::Reviewing,
            Some("local_review_gate".to_string()),
        )
        .to_reason_string(),
    );
    task.scheduler
        .mark_terminal(&task_runner::TaskStatus::Failed);
    state.core.tasks.insert(&task).await;

    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .with_state(state);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/tasks?status=failed")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed = response_json(list_response).await?;
    let listed_task = listed["data"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["id"] == task_id.as_str()))
        .ok_or_else(|| anyhow::anyhow!("stalled task should be listed"))?;
    assert_eq!(listed_task["status"], "failed");
    assert_eq!(listed_task["phase"], "terminal");
    assert_eq!(listed_task["terminal"]["classification"], "stalled");
    assert_eq!(listed_task["terminal"]["reason"], "round_budget_exhausted");
    assert_eq!(listed_task["terminal"]["rounds_used"], 1);
    assert_eq!(listed_task["terminal"]["last_status"], "reviewing");
    assert_eq!(listed_task["terminal"]["waiting_on"], "local_review_gate");

    let detail_response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail = response_json(detail_response).await?;
    assert_eq!(detail["terminal"]["classification"], "stalled");
    assert_eq!(detail["terminal"]["reason"], "round_budget_exhausted");
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
async fn runtime_submission_routes_do_not_consult_legacy_task_store() -> anyhow::Result<()> {
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
    let legacy_task_id = task_runner::TaskId::from_str("legacy-only-task");
    state
        .core
        .tasks
        .insert(&task_runner::TaskState::new(legacy_task_id.clone()))
        .await;

    let app = Router::new()
        .route(
            "/api/workflows/runtime/submissions",
            get(task_query_routes::list_runtime_submissions)
                .post(task_routes::create_runtime_submission),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}",
            get(task_query_routes::get_runtime_submission),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}/artifacts",
            get(runtime_submission_routes::get_artifacts),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}/prompts",
            get(runtime_submission_routes::get_prompts),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}/proof",
            get(task_query_routes::get_runtime_submission_proof),
        )
        .with_state(state.clone());

    let legacy_duplicate_request = task_runner::CreateTaskRequest {
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.canonicalize()?),
        external_id: Some("issue:42".to_string()),
        ..Default::default()
    };
    task_runner::register_pending_task(state.core.tasks.clone(), &legacy_duplicate_request).await;
    let legacy_duplicate_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/submissions")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "repo": "owner/repo",
                        "issue": 42
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(legacy_duplicate_response.status(), StatusCode::CONFLICT);
    let legacy_duplicate = response_json(legacy_duplicate_response).await?;
    assert_eq!(legacy_duplicate["error"], "legacy_submission_active");
    assert!(legacy_duplicate.get("task_id").is_none());
    assert!(legacy_duplicate.get("submission_id").is_none());

    let request_body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "exercise runtime-only submission routes",
        "source": "dashboard",
        "external_id": "manual:runtime-route-test"
    });
    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/submissions")
                .header("content-type", "application/json")
                .body(Body::from(request_body.to_string()))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let created = response_json(create_response).await?;
    assert_eq!(created["execution_path"], "workflow_runtime");
    let submission_id = created["submission_id"]
        .as_str()
        .expect("runtime submission should expose a stable handle")
        .to_string();
    let declarative_submission_id = "declarative-submission-handle";
    let declarative = WorkflowInstance::new(
        "custom_dashboard_flow",
        1,
        "running",
        WorkflowSubject::new("prompt", "custom-dashboard-flow"),
    )
    .with_id("custom-dashboard-flow-instance")
    .with_data(serde_json::json!({
        "project_id": project_root.canonicalize()?.to_string_lossy(),
        "submission_id": declarative_submission_id,
        "definition_hash": "sha256:declarative-test-definition@1",
        "prompt_summary": "custom declarative submission"
    }));
    state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured")
        .upsert_instance(&declarative)
        .await?;

    // Keep two newer issue rows ahead of the prompt rows. A kind filter applied
    // after LIMIT would discard both and incorrectly return an empty page.
    for offset in 1..=2 {
        let mut issue = WorkflowInstance::new(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "done",
            WorkflowSubject::new("github_issue", format!("issue-{offset}")),
        )
        .with_id(format!("newer-issue-instance-{offset}"))
        .with_data(serde_json::json!({
            "project_id": project_root.canonicalize()?.to_string_lossy(),
            "submission_id": format!("newer-issue-submission-{offset}"),
            "issue_number": offset,
            "repo": "owner/repo"
        }));
        issue.created_at = declarative.created_at + chrono::Duration::seconds(offset);
        issue.updated_at = issue.created_at;
        state
            .core
            .workflow_runtime_store
            .as_ref()
            .expect("workflow runtime store should be configured")
            .upsert_instance(&issue)
            .await?;
    }

    let prompt_page_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions?kind=prompt&limit=1")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(prompt_page_response.status(), StatusCode::OK);
    let prompt_page = response_json(prompt_page_response).await?;
    let prompt_rows = prompt_page["data"]
        .as_array()
        .expect("filtered runtime submission page should be an array");
    assert_eq!(prompt_rows.len(), 1);
    assert_eq!(prompt_rows[0]["task_kind"], "prompt");
    assert_eq!(prompt_page["page"]["has_more"], true);

    for (filter, expected_has_more) in [("active=true", true), ("status=implementing", false)] {
        let active_page_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/workflows/runtime/submissions?{filter}&limit=1"
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(active_page_response.status(), StatusCode::OK);
        let active_page = response_json(active_page_response).await?;
        let active_rows = active_page["data"]
            .as_array()
            .expect("active runtime submission page should be an array");
        assert_eq!(active_rows.len(), 1, "filter: {filter}");
        assert_eq!(active_rows[0]["task_kind"], "prompt", "filter: {filter}");
        if filter == "status=implementing" {
            assert_eq!(active_rows[0]["status"], "implementing");
        }
        assert_eq!(
            active_page["page"]["has_more"], expected_has_more,
            "filter: {filter}"
        );
    }

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed = response_json(list_response).await?;
    let listed_ids = listed["data"]
        .as_array()
        .expect("runtime submission list should be an array")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect::<Vec<_>>();
    assert!(listed_ids.contains(&submission_id.as_str()));
    assert!(listed_ids.contains(&declarative_submission_id));
    assert!(!listed_ids.contains(&legacy_task_id.as_str()));

    let declarative_detail_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{declarative_submission_id}"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(declarative_detail_response.status(), StatusCode::OK);
    let declarative_detail = response_json(declarative_detail_response).await?;
    assert_eq!(declarative_detail["task_kind"], "prompt");
    assert_eq!(declarative_detail["execution_path"], "workflow_runtime");

    let detail_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{submission_id}"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail = response_json(detail_response).await?;
    assert_eq!(detail["submission_id"], submission_id);
    assert_eq!(detail["execution_path"], "workflow_runtime");

    let legacy_detail_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{}",
                    legacy_task_id.as_str()
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(legacy_detail_response.status(), StatusCode::NOT_FOUND);

    let prompts_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{submission_id}/prompts"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(prompts_response.status(), StatusCode::OK);
    let prompts = response_json(prompts_response).await?;
    assert_eq!(
        prompts[0]["prompt"],
        "exercise runtime-only submission routes"
    );

    let artifacts_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{submission_id}/artifacts"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(artifacts_response.status(), StatusCode::OK);
    assert_eq!(
        response_json(artifacts_response).await?,
        serde_json::json!([])
    );

    let proof_response = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{submission_id}/proof"
                ))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(proof_response.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
