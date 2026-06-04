use super::*;

async fn record_runtime_local_review_passed(
    runtime_store: &harness_workflow::runtime::WorkflowRuntimeStore,
    project_root: &std::path::Path,
    task_id: &TaskId,
) {
    crate::workflow_runtime_pr_feedback::record_local_review_passed(
        Some(runtime_store),
        crate::workflow_runtime_pr_feedback::LocalReviewPassedRuntimeContext {
            project_root,
            repo: Some("owner/repo"),
            issue_number: Some(123),
            task_id,
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            summary: "Local agent review approved the PR.",
        },
    )
    .await;
}

#[tokio::test]
async fn hosted_bot_disabled_completion_requires_validation_pass() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["true".to_string()];

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        None,
        dir.path(),
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/1"),
        Instant::now(),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Done);
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "local_review_validation" && round.result == "passed"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_completion_blocks_validation_failure() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["false".to_string()];

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        None,
        dir.path(),
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/1"),
        Instant::now(),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("validation failed"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "local_review_validation" && round.result == "failed"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_validation_failure_treats_merged_pr_as_done() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["false".to_string()];
    let req = CreateTaskRequest {
        issue: Some(123),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: None,
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Failed("PR checks should not run after merge"),
            pr_head: LocalReviewPrHead::Failed("PR head should not run after merge"),
            pr_state: LocalReviewPrState::Merged,
        }),
        &project_root,
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/77"),
        Instant::now(),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Done);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("merged externally"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state.rounds.iter().any(|round| {
        round.action == "local_review_validation" && round.result == "skipped_merged"
    }));
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_open_gate" && round.result == "merged"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_validation_failure_records_blocking_feedback() -> anyhow::Result<()> {
    let Ok(database_url) = harness_core::db::resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let runtime_store =
        match harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await
        {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let req = CreateTaskRequest {
        issue: Some(123),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["false".to_string()];

    record_runtime_local_review_passed(&runtime_store, &project_root, &task_id).await;

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: Some(&runtime_store),
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::Verified,
            pr_state: LocalReviewPrState::Open,
        }),
        &project_root,
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/77"),
        Instant::now(),
    )
    .await?;

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow instance should be recorded");
    assert_eq!(instance.state, "addressing_feedback");
    let events = runtime_store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "FeedbackFound"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_completion_records_ready_to_merge_feedback() -> anyhow::Result<()> {
    let Ok(database_url) = harness_core::db::resolve_database_url(None) else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let runtime_store =
        match harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            dir.path(),
            Some(&database_url),
        )
        .await
        {
            Ok(store) => store,
            Err(_) => return Ok(()),
        };
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let req = CreateTaskRequest {
        issue: Some(123),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["true".to_string()];

    record_runtime_local_review_passed(&runtime_store, &project_root, &task_id).await;

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: Some(&runtime_store),
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::Verified,
            pr_state: LocalReviewPrState::Open,
        }),
        &project_root,
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/77"),
        Instant::now(),
    )
    .await?;

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        123,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("workflow instance should be recorded");
    assert_eq!(instance.state, "ready_to_merge");
    let events = runtime_store.events_for(&workflow_id).await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "PrReadyToMerge"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_rechecks_pr_state_after_ci_polling() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let req = CreateTaskRequest {
        issue: Some(123),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };
    let pr_states = std::sync::Arc::new(std::sync::Mutex::new(vec![
        Ok(review_loop::PrOpenOrMergedState::Open),
        Ok(review_loop::PrOpenOrMergedState::Merged),
    ]));
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["true".to_string()];

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: None,
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::Verified,
            pr_state: LocalReviewPrState::Sequence(pr_states),
        }),
        &project_root,
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/77"),
        Instant::now(),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Done);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("merged externally"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_open_gate" && round.result == "merged"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_blocks_pr_head_change_after_local_approval() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let req = CreateTaskRequest {
        issue: Some(123),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["true".to_string()];

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: None,
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::FinalFailed(
                "GitHub PR head changed after local review approval for owner/repo#77",
            ),
            pr_state: LocalReviewPrState::Open,
        }),
        &project_root,
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/77"),
        Instant::now(),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("PR head changed after local approval"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_head_gate" && round.result == "failed"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_blocks_pr_head_change_during_no_fix_review() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let req = CreateTaskRequest {
        issue: Some(123),
        repo: Some("owner/repo".to_string()),
        ..CreateTaskRequest::default()
    };
    let mut project_config = harness_core::config::project::ProjectConfig::default();
    project_config.validation.pre_push = vec!["true".to_string()];

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: None,
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::GitHub {
                repo_slug: "owner/repo",
                github_token: None,
                before_review_sha: Some("base-sha"),
                before_review_error: None,
                approved_review_sha: Some("new-sha"),
                approved_review_error: None,
                local_fix_requires_pr_head_advance: false,
            },
            pr_state: LocalReviewPrState::Open,
        }),
        &project_root,
        &project_config,
        &HashMap::new(),
        4,
        Some("https://github.com/owner/repo/pull/77"),
        Instant::now(),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("could not prove the reviewed PR head is valid"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state.rounds.iter().any(|round| {
        round.action == "pr_head_gate"
            && round.result == "failed"
            && round
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("changed during local review"))
    }));
    assert!(!state
        .rounds
        .iter()
        .any(|round| round.action == "pr_ci_gate"));
    Ok(())
}
