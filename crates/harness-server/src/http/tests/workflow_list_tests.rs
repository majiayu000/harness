use super::*;

#[tokio::test]
async fn list_tasks_enriches_workflows_for_issue_and_pr_tasks() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = crate::test_helpers::tempdir_in_home("harness-test-task-workflows-")?;
    let mut state = make_test_state(dir.path()).await?;
    let workflow_store = Arc::new(
        harness_workflow::issue_lifecycle::IssueWorkflowStore::open(
            &harness_core::config::dirs::default_db_path(dir.path(), "issue_workflows"),
        )
        .await?,
    );
    let project_id = dir.path().display().to_string();

    workflow_store
        .record_issue_scheduled(
            project_id.as_str(),
            Some("owner/repo"),
            42,
            "task-issue",
            &[],
            false,
        )
        .await?;
    workflow_store
        .record_issue_scheduled(
            project_id.as_str(),
            Some("owner/repo"),
            77,
            "task-pr",
            &[],
            false,
        )
        .await?;
    workflow_store
        .record_pr_detected(
            project_id.as_str(),
            Some("owner/repo"),
            77,
            "task-pr",
            101,
            "https://github.com/owner/repo/pull/101",
        )
        .await?;
    workflow_store
        .record_issue_scheduled(
            "/tmp/other-project",
            Some("owner/repo"),
            42,
            "other-task",
            &[],
            false,
        )
        .await?;

    Arc::get_mut(&mut state)
        .expect("state should be uniquely owned")
        .core
        .issue_workflow_store = Some(workflow_store);

    let issue_task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:42".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        description: Some("issue task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Implement,
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
    let pr_task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: Some("https://github.com/owner/repo/pull/101".to_string()),
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: Some(77),
        repo: Some("owner/repo".to_string()),
        description: Some("pr task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Implement,
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
    let issue_task_id = issue_task.id.0.clone();
    let pr_task_id = pr_task.id.0.clone();
    state.core.tasks.insert(&issue_task).await;
    state.core.tasks.insert(&pr_task).await;

    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .with_state(state);

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let tasks: serde_json::Value = serde_json::from_slice(&body)?;
    let tasks = tasks["data"].as_array().expect("tasks array");

    let issue_json = tasks
        .iter()
        .find(|task| task["id"] == issue_task_id)
        .expect("issue task should be listed");
    assert_eq!(issue_json["workflow"]["project_id"], project_id);
    assert_eq!(issue_json["workflow"]["issue_number"], 42);
    assert_eq!(issue_json["workflow"]["pr_number"], serde_json::Value::Null);

    let pr_json = tasks
        .iter()
        .find(|task| task["id"] == pr_task_id)
        .expect("pr task should be listed");
    assert_eq!(pr_json["workflow"]["project_id"], project_id);
    assert_eq!(pr_json["workflow"]["issue_number"], 77);
    assert_eq!(pr_json["workflow"]["pr_number"], 101);

    Ok(())
}

#[tokio::test]
async fn list_tasks_exposes_workflow_fallback_metadata() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_issue_workflows(dir.path()).await?;
    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .with_state(state.clone());

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Done,
        turn: 3,
        pr_url: Some("https://github.com/owner/repo/pull/501".to_string()),
        rounds: vec![],
        error: Some("Review fallback tier C via silence".to_string()),
        source: Some("github".to_string()),
        external_id: Some("issue:945".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: Some(945),
        repo: Some("owner/repo".to_string()),
        description: Some("issue #945".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Terminal,
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
    state.core.tasks.insert(&task).await;
    let workflows = state
        .core
        .issue_workflow_store
        .as_ref()
        .expect("workflow store");
    workflows
        .record_issue_scheduled(
            &dir.path().to_string_lossy(),
            Some("owner/repo"),
            945,
            task.id.as_str(),
            &[],
            false,
        )
        .await?;
    workflows
        .record_pr_detected(
            &dir.path().to_string_lossy(),
            Some("owner/repo"),
            945,
            task.id.as_str(),
            501,
            "https://github.com/owner/repo/pull/501",
        )
        .await?;
    workflows
        .record_ready_to_merge_with_fallback(
            &dir.path().to_string_lossy(),
            Some("owner/repo"),
            501,
            Some("fallback via silence"),
            harness_workflow::issue_lifecycle::ReviewFallbackSnapshot {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence,
                active_bot: Some("codex".to_string()),
                activated_at: Utc::now(),
            },
        )
        .await?;

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let tasks: serde_json::Value = serde_json::from_slice(&body)?;
    let task = tasks
        .get("data")
        .and_then(serde_json::Value::as_array)
        .and_then(|tasks| tasks.first())
        .expect("task row");
    assert_eq!(task["workflow"]["state"], "ready_to_merge");
    assert_eq!(task["workflow"]["review_fallback"]["tier"], "c");
    assert_eq!(task["workflow"]["review_fallback"]["trigger"], "silence");
    assert_eq!(task["workflow"]["review_fallback"]["active_bot"], "codex");

    let detail_response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task["id"].as_str().expect("task id")))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail_body = axum::body::to_bytes(detail_response.into_body(), usize::MAX).await?;
    let detail: serde_json::Value = serde_json::from_slice(&detail_body)?;
    assert_eq!(detail["workflow"]["state"], "ready_to_merge");
    assert_eq!(detail["workflow"]["review_fallback"]["tier"], "c");
    assert!(detail["workflow"].get("events").is_none());
    Ok(())
}
