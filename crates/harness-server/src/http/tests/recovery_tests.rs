use super::*;

async fn wait_for_task_status(
    state: &Arc<AppState>,
    task_id: &task_runner::TaskId,
    expected: task_runner::TaskStatus,
) -> anyhow::Result<task_runner::TaskState> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = state.core.tasks.get_with_db_fallback(task_id).await? {
            if task.status == expected {
                return Ok(task);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "task {} did not reach status {:?} within 5 seconds",
                task_id.0,
                expected
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn wait_for_task_to_leave_pending(
    state: &Arc<AppState>,
    task_id: &task_runner::TaskId,
) -> anyhow::Result<task_runner::TaskState> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = state.core.tasks.get_with_db_fallback(task_id).await? {
            if task.status != task_runner::TaskStatus::Pending {
                return Ok(task);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("task {} did not leave Pending within 5 seconds", task_id.0);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn orphan_issue_task_is_redispatched_using_existing_task_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_pr_task_is_redispatched_using_existing_task_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            pr: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("pr:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_issue_task_is_redispatched_when_external_id_is_noncanonical() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("legacy-issue-944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_pr_task_is_redispatched_when_external_id_is_noncanonical() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            pr: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("legacy-pr-944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_issue_task_with_prompt_context_fails_when_identifier_is_missing(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let mut settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            prompt: Some("extra issue context".to_string()),
            ..task_runner::CreateTaskRequest::default()
        });
    settings.issue = None;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("legacy-issue-without-number".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state =
        wait_for_task_status(&state, &task_id, task_runner::TaskStatus::Failed).await?;
    assert_eq!(
        final_state.error.as_deref(),
        Some("orphaned issue task: issue number not persisted")
    );
    assert!(agent.prompts.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn orphan_prompt_only_task_fails_closed_when_prompt_was_not_persisted() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

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
        project_root: Some(dir.path().to_path_buf()),
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

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state =
        wait_for_task_status(&state, &task_id, task_runner::TaskStatus::Failed).await?;
    assert_eq!(
        final_state.error.as_deref(),
        Some("orphaned prompt-only task: prompt not persisted")
    );
    assert!(agent.prompts.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn orphan_prompt_only_task_waits_for_runtime_host_lease_to_expire_before_failing(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let mut task = task_runner::TaskState {
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
        project_root: Some(dir.path().to_path_buf()),
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
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::milliseconds(75),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state =
        wait_for_task_status(&state, &task_id, task_runner::TaskStatus::Failed).await?;
    assert_eq!(
        final_state.error.as_deref(),
        Some("orphaned prompt-only task: prompt not persisted")
    );
    assert_eq!(final_state.scheduler.runtime_host_id(), None);
    assert!(matches!(
        final_state.scheduler.authority_state,
        task_runner::SchedulerAuthorityState::Failed
    ));
    assert!(agent.prompts.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn orphan_recovery_excludes_pr_checkpoint_and_non_pending_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let orphan = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: Some(944),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("issue #944".to_string()),
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
    let orphan_id = orphan.id.clone();
    state.core.tasks.insert(&orphan).await;

    let with_pr = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: Some("https://github.com/majiayu000/harness/pull/944".to_string()),
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("pr:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: Some("PR #944".to_string()),
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
    let with_pr_id = with_pr.id.clone();
    state.core.tasks.insert(&with_pr).await;

    let with_checkpoint = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:945".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: Some(945),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("issue #945".to_string()),
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
    let with_checkpoint_id = with_checkpoint.id.clone();
    state.core.tasks.insert(&with_checkpoint).await;
    state
        .core
        .tasks
        .write_checkpoint(&with_checkpoint_id, None, Some("plan output"), None, "plan")
        .await?;

    let failed = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Failed,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: Some("already failed".to_string()),
        source: Some("github".to_string()),
        external_id: Some("issue:946".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: Some(946),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("issue #946".to_string()),
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
    let failed_id = failed.id.clone();
    state.core.tasks.insert(&failed).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_orphan = wait_for_task_to_leave_pending(&state, &orphan_id).await?;
    assert_eq!(final_orphan.id, orphan_id);
    assert_ne!(final_orphan.status, task_runner::TaskStatus::Pending);
    assert!(agent.prompts.lock().await.len() <= 1);

    let pr_state = state
        .core
        .tasks
        .get_with_db_fallback(&with_pr_id)
        .await?
        .expect("pr task should exist");
    assert_eq!(pr_state.status, task_runner::TaskStatus::Pending);

    let checkpoint_state = state
        .core
        .tasks
        .get_with_db_fallback(&with_checkpoint_id)
        .await?
        .expect("checkpoint task should exist");
    assert_eq!(checkpoint_state.status, task_runner::TaskStatus::Pending);

    let failed_state = state
        .core
        .tasks
        .get_with_db_fallback(&failed_id)
        .await?
        .expect("failed task should exist");
    assert_eq!(failed_state.status, task_runner::TaskStatus::Failed);
    Ok(())
}
