use super::*;

#[tokio::test]
async fn pr_recovery_marks_task_failed_when_pr_url_unparseable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: Some("not-a-valid-pr-url".to_string()),
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_pr_recovery(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("task was not marked Failed within 5 seconds after pr_recovery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert!(matches!(
        final_state.status,
        task_runner::TaskStatus::Failed
    ));
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("unparseable pr_url"),
        "error should mention unparseable pr_url, got: {:?}",
        final_state.error
    );
    Ok(())
}

#[tokio::test]
async fn pr_recovery_redispatches_prompt_tasks_with_pr_urls() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Prompt,
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: Some("not-a-valid-pr-url".to_string()),
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
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
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_pr_recovery(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("prompt task was not re-dispatched within 5 seconds after pr_recovery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert!(matches!(
        final_state.status,
        task_runner::TaskStatus::Failed
    ));
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("unparseable pr_url"),
        "error should mention unparseable pr_url, got: {:?}",
        final_state.error
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_recovery_marks_prompt_task_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Prompt,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: Some("prompt task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;
    // Write a checkpoint so the task appears in pending_tasks_with_checkpoint().
    state
        .core
        .tasks
        .write_checkpoint(&task_id, None, Some("plan output"), None, "plan")
        .await?;

    super::background::spawn_checkpoint_recovery(&state).await;

    // The spawned tokio task updates the cache; give it a moment to complete.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("prompt task was not marked Failed within 5 seconds after checkpoint_recovery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert!(matches!(
        final_state.status,
        task_runner::TaskStatus::Failed
    ));
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("prompt task"),
        "error should mention prompt task recovery failure, got: {:?}",
        final_state.error
    );
    Ok(())
}

#[test]
fn recovery_queue_domain_routes_review_tasks_to_review_capacity() {
    assert_eq!(
        super::background::recovery_queue_domain(task_runner::TaskKind::Review),
        super::task_routes::QueueDomain::Review
    );
    assert_eq!(
        super::background::recovery_queue_domain(task_runner::TaskKind::Planner),
        super::task_routes::QueueDomain::Primary
    );
}

#[tokio::test]
async fn get_task_exposes_workspace_lifecycle_metadata() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = crate::test_helpers::tempdir_in_home("harness-test-task-metadata-")?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        status: task_runner::TaskStatus::Failed,
        failure_kind: Some(task_runner::TaskFailureKind::WorkspaceLifecycle),
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: Some("workspace lifecycle reconciliation failed".to_string()),
        source: Some("github".to_string()),
        external_id: Some("issue:899".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: Some(dir.path().join("workspaces/task-899")),
        workspace_owner: Some("session-899".to_string()),
        run_generation: 3,
        issue: Some(899),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("workspace failure".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        task_kind: task_runner::TaskKind::Issue,
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    let app = runtime_submission_app(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/workflows/runtime/submissions/{}", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let json = response_json(response).await?;
    assert_eq!(json["failure_kind"], "workspace_lifecycle");
    assert_eq!(json["workspace_owner"], "session-899");
    assert_eq!(json["run_generation"], 3);
    assert!(json["workspace_path"]
        .as_str()
        .unwrap_or("")
        .ends_with("workspaces/task-899"));
    Ok(())
}

#[tokio::test]
async fn pr_recovery_waits_for_runtime_host_lease_to_expire() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let mut task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::default(),
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: Some("not-a-valid-pr-url".to_string()),
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
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
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::milliseconds(75),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_pr_recovery(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("task was not recovered within 5 seconds after lease expiry");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert_eq!(final_state.scheduler.runtime_host_id(), None);
    assert!(matches!(
        final_state.scheduler.authority_state,
        task_runner::SchedulerAuthorityState::Failed
    ));
    Ok(())
}

#[tokio::test]
async fn checkpoint_recovery_waits_for_runtime_host_lease_to_expire() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let mut task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::default(),
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: Some("prompt task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
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
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::milliseconds(75),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;
    state
        .core
        .tasks
        .write_checkpoint(&task_id, None, Some("plan output"), None, "plan")
        .await?;

    super::background::spawn_checkpoint_recovery(&state).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("checkpoint task was not recovered within 5 seconds after lease expiry");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert_eq!(final_state.scheduler.runtime_host_id(), None);
    assert!(matches!(
        final_state.scheduler.authority_state,
        task_runner::SchedulerAuthorityState::Failed
    ));
    Ok(())
}
