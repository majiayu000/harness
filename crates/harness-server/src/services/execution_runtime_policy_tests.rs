use super::*;

#[test]
fn remote_subject_gate_rejects_closed_and_unknown_state() {
    assert!(validate_remote_subject_open(
        "PR #7",
        "owner/repo",
        crate::reconciliation::GitHubState::Open,
    )
    .is_ok());

    let closed = validate_remote_subject_open(
        "PR #7",
        "owner/repo",
        crate::reconciliation::GitHubState::PrMerged,
    )
    .expect_err("merged PR must be rejected");
    assert!(matches!(closed, EnqueueTaskError::BadRequest(_)));

    let unknown = validate_remote_subject_open(
        "issue #9",
        "owner/repo",
        crate::reconciliation::GitHubState::Unknown,
    )
    .expect_err("unverified issue state must fail closed");
    assert!(matches!(unknown, EnqueueTaskError::Internal(_)));
}

#[tokio::test]
async fn remote_subject_gate_rejects_before_workflow_or_job_creation() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _env_guard = crate::workspace::test_support::async_env_lock()
        .lock()
        .await;
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            dir.path(),
            Some(&crate::test_helpers::test_database_url()?),
        )
        .await?,
    );
    let service = DefaultExecutionService::new(
        Arc::new(HarnessConfig::default()),
        Some(Arc::clone(&store)),
        None,
        Vec::new(),
    );

    let merged_pr_api = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/pulls/7",
        r#"{"number":7,"state":"closed","merged_at":"2026-08-01T00:00:00Z"}"#,
    )
    .await;
    let api_guard = crate::workspace::test_support::ScopedEnvVar::set(
        "HARNESS_GITHUB_API_BASE_URL",
        &merged_pr_api,
    );
    let merged_error = service
        .enqueue(CreateTaskRequest {
            pr: Some(7),
            repo: Some("owner/repo".to_string()),
            project: Some(dir.path().to_path_buf()),
            ..CreateTaskRequest::default()
        })
        .await
        .expect_err("merged PR must be rejected before submission");
    assert!(matches!(merged_error, EnqueueTaskError::BadRequest(_)));
    drop(api_guard);

    let closed_issue_api = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/9",
        r#"{"number":9,"state":"closed","state_reason":"completed"}"#,
    )
    .await;
    let api_guard = crate::workspace::test_support::ScopedEnvVar::set(
        "HARNESS_GITHUB_API_BASE_URL",
        &closed_issue_api,
    );
    let closed_error = service
        .enqueue(CreateTaskRequest {
            issue: Some(9),
            repo: Some("owner/repo".to_string()),
            project: Some(dir.path().to_path_buf()),
            ..CreateTaskRequest::default()
        })
        .await
        .expect_err("closed issue must be rejected before submission");
    assert!(matches!(closed_error, EnqueueTaskError::BadRequest(_)));
    drop(api_guard);

    let wrong_number_api = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/pulls/11",
        r#"{"number":12,"state":"open","merged_at":null}"#,
    )
    .await;
    let api_guard = crate::workspace::test_support::ScopedEnvVar::set(
        "HARNESS_GITHUB_API_BASE_URL",
        &wrong_number_api,
    );
    let unknown_error = service
        .enqueue(CreateTaskRequest {
            pr: Some(11),
            repo: Some("owner/repo".to_string()),
            project: Some(dir.path().to_path_buf()),
            ..CreateTaskRequest::default()
        })
        .await
        .expect_err("wrong-number response must fail closed");
    assert!(matches!(unknown_error, EnqueueTaskError::Internal(_)));
    drop(api_guard);

    let malformed_error = service
        .enqueue(CreateTaskRequest {
            pr: Some(13),
            repo: Some("owner/repo/extra?target=other".to_string()),
            project: Some(dir.path().to_path_buf()),
            ..CreateTaskRequest::default()
        })
        .await
        .expect_err("malformed repo slug must fail before a remote request");
    assert!(matches!(malformed_error, EnqueueTaskError::BadRequest(_)));

    assert!(store
        .list_instances_by_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            None,
            None,
        )
        .await?
        .is_empty());
    assert!(store
        .list_instances_by_definition(
            harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            None,
            None,
        )
        .await?
        .is_empty());
    Ok(())
}
use crate::workflow_runtime_submission::{
    prompt_execution_policy,
    runtime_models::{PromptExecutionPolicy, TaskKind},
    runtime_request::SystemTaskInput,
};

#[tokio::test]
async fn review_submission_preserves_runtime_execution_policy() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            dir.path(),
            Some(&crate::test_helpers::test_database_url()?),
        )
        .await?,
    );
    let service = DefaultExecutionService::new_for_tests(
        Arc::new(HarnessConfig::default()),
        Some(store.clone()),
        None,
        Vec::new(),
    );
    let prompt = "Review commits since the persisted watermark.".to_string();
    let task_id = service
        .enqueue_in_domain(
            CreateTaskRequest {
                prompt: Some(prompt.clone()),
                agent: Some("claude".to_string()),
                project: Some(dir.path().to_path_buf()),
                turn_timeout_secs: 91,
                source: Some("periodic_review".to_string()),
                external_id: Some("periodic-review:policy-test".to_string()),
                priority: 2,
                system_input: Some(SystemTaskInput::PeriodicReview { prompt }),
                ..CreateTaskRequest::default()
            },
            QueueDomain::Review,
        )
        .await?;

    let workflow = store
        .get_instance_by_submission_id(task_id.as_str())
        .await?
        .expect("review workflow should be persisted");
    assert_eq!(
        prompt_execution_policy(&workflow.data)?,
        Some(PromptExecutionPolicy {
            task_kind: TaskKind::Review,
            agent: Some("claude".to_string()),
            turn_timeout_secs: Some(91),
            queue_domain: QueueDomain::Review,
            priority: 2,
        })
    );
    Ok(())
}
