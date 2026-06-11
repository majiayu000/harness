use super::*;
use harness_core::review::{ReviewGateDecision, ReviewGateResult};

#[tokio::test]
async fn hosted_bot_disabled_completion_blocks_pr_ci_failure() -> anyhow::Result<()> {
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
            pr_checks: LocalReviewPrChecks::Failed("GitHub PR checks are not green"),
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

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("PR CI checks are not green"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_ci_gate" && round.result == "failed"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_treats_merged_pr_as_done_after_ci_failure() -> anyhow::Result<()> {
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
            pr_checks: LocalReviewPrChecks::Failed("GitHub PR checks are not green"),
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
    assert!(!state
        .rounds
        .iter()
        .any(|round| round.action == "pr_ci_gate" && round.result == "failed"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_completion_blocks_stale_pr_head_after_local_fix() -> anyhow::Result<()>
{
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
            pr_head: LocalReviewPrHead::Failed(
                "GitHub PR head did not advance after local review fix for owner/repo#77",
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
            .contains("could not prove the reviewed PR head is valid"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_head_gate" && round.result == "failed"));
    assert!(!state
        .rounds
        .iter()
        .any(|round| round.action == "pr_ci_gate"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_pr_head_lookup_failure_does_not_record_feedback() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let runtime_store = match harness_core::db::resolve_database_url(None) {
        Ok(database_url) => {
            harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
                dir.path(),
                Some(&database_url),
            )
            .await
            .ok()
        }
        Err(_) => None,
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

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: runtime_store.as_ref(),
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::Failed(
                "GitHub token is required to verify PR head when hosted review is disabled",
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
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_head_gate" && round.result == "failed"));
    if let Some(runtime_store) = runtime_store {
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        assert!(
            runtime_store.get_instance(&workflow_id).await?.is_none(),
            "PR head lookup failures must not become PR feedback remediation workflows"
        );
    }
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_completion_blocks_closed_pr() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let runtime_store = match harness_core::db::resolve_database_url(None) {
        Ok(database_url) => {
            harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
                dir.path(),
                Some(&database_url),
            )
            .await
            .ok()
        }
        Err(_) => None,
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

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: runtime_store.as_ref(),
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Passed,
            pr_head: LocalReviewPrHead::Verified,
            pr_state: LocalReviewPrState::NotOpen(
                "GitHub PR owner/repo#77 is closed without merge",
            ),
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
            .contains("PR is not open"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_open_gate" && round.result == "failed"));
    if let Some(runtime_store) = runtime_store {
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        assert!(
            runtime_store.get_instance(&workflow_id).await?.is_none(),
            "PR open-state gate failures must not become PR feedback remediation workflows"
        );
    }
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_completion_treats_merged_pr_as_done() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let runtime_store = match harness_core::db::resolve_database_url(None) {
        Ok(database_url) => {
            harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
                dir.path(),
                Some(&database_url),
            )
            .await
            .ok()
        }
        Err(_) => None,
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

    complete_after_local_review_without_hosted_bot(
        &store,
        &task_id,
        Some(LocalReviewReadyToMergeFeedback {
            issue_workflow_store: None,
            workflow_runtime_store: runtime_store.as_ref(),
            project_root: &project_root,
            req: &req,
            pr_num: 77,
            pr_checks: LocalReviewPrChecks::Failed("PR checks should not run after merge"),
            pr_head: LocalReviewPrHead::Failed("PR head gate should not run after merge"),
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
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "pr_open_gate" && round.result == "merged"));
    assert!(!state
        .rounds
        .iter()
        .any(|round| round.action == "pr_ci_gate" && round.result == "failed"));
    if let Some(runtime_store) = runtime_store {
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            123,
        );
        let instance = runtime_store
            .get_instance(&workflow_id)
            .await?
            .expect("workflow instance should be recorded");
        assert_eq!(instance.state, "done");
        let events = runtime_store.events_for(&workflow_id).await?;
        assert!(events.iter().any(|event| event.event_type == "PrMerged"));
    }
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_requires_local_review_approval() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;

    fail_missing_local_review_gate(
        &store,
        &task_id,
        4,
        Some("https://github.com/owner/repo/pull/1"),
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("no local agent review approved"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "review_gate_config" && round.result == "failed"));
    Ok(())
}

#[tokio::test]
async fn hosted_bot_disabled_blocks_missing_required_provider_report() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = TaskId::new();
    store
        .insert(&crate::task_runner::TaskState::new(task_id.clone()))
        .await;
    let gate = ReviewGateResult {
        decision: ReviewGateDecision::Blocked,
        blocking_provider_id: Some("codex_cli_review".to_string()),
        summary: "Required review provider did not produce a report.".to_string(),
    };

    fail_review_provider_gate(
        &store,
        &task_id,
        4,
        Some("https://github.com/owner/repo/pull/1"),
        &gate,
    )
    .await?;

    let state = store.get(&task_id).expect("task state should be present");
    assert_eq!(state.status, TaskStatus::Failed);
    assert!(
        state
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("codex_cli_review"),
        "unexpected error: {:?}",
        state.error
    );
    assert!(state
        .rounds
        .iter()
        .any(|round| round.action == "review_provider_gate" && round.result == "failed"));
    Ok(())
}
