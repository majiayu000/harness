use super::*;

async fn concurrent_github_state_server(
    path: &'static str,
    body: &'static str,
    request_count: usize,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind concurrent GitHub mock");
    let addr = listener.local_addr().expect("GitHub mock address");
    tokio::spawn(async move {
        let mut sockets = Vec::with_capacity(request_count);
        for _ in 0..request_count {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 2048];
            let Ok(n) = socket.read(&mut buf).await else {
                return;
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            if !request.starts_with(&format!("GET {path} ")) {
                return;
            }
            sockets.push(socket);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        for mut socket in sockets {
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    format!("http://{addr}")
}

#[test]
fn remote_subject_gate_accepts_only_open_state() {
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

#[test]
fn remote_subject_gate_validates_repository_slugs() {
    for valid in ["owner/repo", "owner-name/repo_name", "owner.name/repo.rs"] {
        assert!(
            valid_github_repo_slug(valid),
            "expected valid slug: {valid}"
        );
    }
    for invalid in [
        "owner",
        "owner/repo/extra",
        "owner/../repo",
        "owner/repo?target=other",
        "/repo",
        "owner/",
        " owner/repo ",
    ] {
        assert!(
            !valid_github_repo_slug(invalid),
            "expected invalid slug: {invalid}"
        );
    }
    assert!(!valid_github_repo_slug(&format!("{}/repo", "a".repeat(40))));
    assert!(!valid_github_repo_slug(&format!(
        "owner/{}",
        "a".repeat(101)
    )));
}

#[test]
fn remote_subject_identity_normalizes_repository_case() {
    let mut req = CreateTaskRequest {
        issue: Some(7),
        repo: Some("Owner/Repo".to_string()),
        ..CreateTaskRequest::default()
    };
    DefaultExecutionService::normalize_remote_subject_identity(&mut req)
        .expect("mixed-case GitHub slug should canonicalize");
    assert_eq!(req.repo.as_deref(), Some("owner/repo"));
}

#[tokio::test]
async fn remote_subject_gate_rejects_before_workflow_creation() -> anyhow::Result<()> {
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
    let project_root = std::fs::canonicalize(dir.path())?;
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

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/9",
        r#"{"number":9,"repository_url":"https://api.github.test/repos/owner/repo","state":"closed","state_reason":"completed"}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let error = service
        .enqueue(CreateTaskRequest {
            issue: Some(9),
            repo: Some("owner/repo".to_string()),
            project: Some(project_root.clone()),
            ..CreateTaskRequest::default()
        })
        .await
        .expect_err("closed issue must be rejected before submission");
    assert!(matches!(error, EnqueueTaskError::BadRequest(_)));
    drop(api_guard);

    assert!(store
        .list_instances_by_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            None,
            None,
        )
        .await?
        .is_empty());

    let existing_issue_task_id = TaskId::from_str("existing-mixed-case-issue-task");
    crate::workflow_runtime_submission::record_issue_submission(
        &store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("Owner/Repo"),
            issue_number: 10,
            task_id: &existing_issue_task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:10"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/issues/10",
        r#"{"number":10,"repository_url":"https://api.github.test/repos/owner/repo","state":"closed","state_reason":"completed"}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let returned_task_id = service
        .enqueue(CreateTaskRequest {
            issue: Some(10),
            repo: Some("owner/repo".to_string()),
            project: Some(project_root.clone()),
            ..CreateTaskRequest::default()
        })
        .await?;
    drop(api_guard);
    assert_eq!(
        returned_task_id, existing_issue_task_id,
        "an active legacy mixed-case issue retry must return without rechecking GitHub"
    );

    let existing_task_id = TaskId::from_str("existing-pr-feedback-task");
    let outcome = crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep_for_pr(
        &store,
        crate::workflow_runtime_pr_feedback::PrFeedbackSweepRuntimeContext {
            project_root: &project_root,
            repo: Some("Owner/Repo"),
            task_id: &existing_task_id,
            pr_number: 7,
            pr_url: None,
        },
    )
    .await?;
    assert!(matches!(
        outcome,
        crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested { .. }
    ));

    let api_base = crate::workspace::test_support::github_state_server(
        "/repos/owner/repo/pulls/7",
        r#"{"number":7,"state":"closed","merged_at":null,"base":{"repo":{"full_name":"owner/repo"}}}"#,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let returned_task_id = service
        .enqueue(CreateTaskRequest {
            pr: Some(7),
            repo: Some("owner/repo".to_string()),
            project: Some(project_root.clone()),
            ..CreateTaskRequest::default()
        })
        .await?;
    drop(api_guard);
    assert_eq!(
        returned_task_id, existing_task_id,
        "an active legacy mixed-case PR retry must return without rechecking GitHub"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_remote_subject_admissions_return_one_issue_handle() -> anyhow::Result<()> {
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
    let project_root = std::fs::canonicalize(dir.path())?;
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
    let api_base = concurrent_github_state_server(
        "/repos/owner/repo/issues/10",
        r#"{"number":10,"repository_url":"https://api.github.test/repos/owner/repo","state":"open"}"#,
        2,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let request = CreateTaskRequest {
        issue: Some(10),
        repo: Some("Owner/Repo".to_string()),
        project: Some(project_root.clone()),
        ..CreateTaskRequest::default()
    };
    let (first, second) = tokio::join!(service.enqueue(request.clone()), service.enqueue(request));
    drop(api_guard);
    assert_eq!(first?, second?);

    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &project_root.to_string_lossy(),
        Some("owner/repo"),
        10,
    );
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1, "concurrent admission must enqueue once");
    Ok(())
}

#[tokio::test]
async fn concurrent_remote_subject_admissions_return_one_pr_handle() -> anyhow::Result<()> {
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
    let project_root = std::fs::canonicalize(dir.path())?;
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
    let api_base = concurrent_github_state_server(
        "/repos/owner/repo/pulls/11",
        r#"{"number":11,"state":"open","merged_at":null,"base":{"repo":{"full_name":"owner/repo"}}}"#,
        2,
    )
    .await;
    let api_guard =
        crate::workspace::test_support::ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);
    let request = CreateTaskRequest {
        pr: Some(11),
        repo: Some("Owner/Repo".to_string()),
        project: Some(project_root.clone()),
        ..CreateTaskRequest::default()
    };
    let (first, second) = tokio::join!(service.enqueue(request.clone()), service.enqueue(request));
    drop(api_guard);
    assert_eq!(first?, second?);

    let instance = store
        .get_instance_by_pr(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            &project_root.to_string_lossy(),
            Some("owner/repo"),
            11,
        )
        .await?
        .expect("concurrent PR admission should persist one workflow");
    assert_eq!(
        store.commands_for(&instance.id).await?.len(),
        1,
        "concurrent PR admission must enqueue once"
    );
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
