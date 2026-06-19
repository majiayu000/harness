use super::*;

#[tokio::test]
async fn feishu_webhook_returns_service_unavailable_when_token_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), None).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(feishu_challenge_payload(None).to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = response_json(response).await?;
    assert_eq!(json["error"], "Feishu intake not configured");
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_returns_service_unavailable_when_token_is_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), Some("")).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(
                    feishu_challenge_payload(Some("secret-123")).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = response_json(response).await?;
    assert_eq!(json["error"], "Feishu intake not configured");
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_accepts_challenge_with_valid_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), Some("secret-123")).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(
                    feishu_challenge_payload(Some("secret-123")).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    assert_eq!(json["challenge"], "challenge-123");
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_rejects_invalid_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), Some("secret-123")).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(feishu_event_payload(Some("wrong")).to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = response_json(response).await?;
    assert_eq!(json["error"], "invalid verification token");
    Ok(())
}

#[tokio::test]
async fn webhook_issues_opened_with_mention_schedules_runtime_issue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "org/repo".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/repo" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "planning");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_issue_submission(
        &state,
        dir.path(),
        Some("org/repo"),
        77,
        json["task_id"].as_str().expect("task id should be present"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn webhook_issues_opened_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "org/repo".to_string(),
        ..Default::default()
    });
    let (state, _agent) =
        make_test_state_with_agent_and_config(dir.path(), dir.path(), config).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/repo" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = response_json(response).await?;
    assert!(
        json["error"].as_str().is_some_and(|error| error
            .contains("workflow runtime store is required for GitHub issue submissions")),
        "unexpected response: {json}"
    );
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn webhook_routes_runtime_issue_to_repo_specific_project_root() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let repo_a_dir = crate::test_helpers::tempdir_in_home("webhook-repo-a-")?;
    let repo_b_dir = crate::test_helpers::tempdir_in_home("webhook-repo-b-")?;
    let secret = "secret";

    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "org/repo-b".to_string(),
            label: "harness".to_string(),
            project_root: Some(repo_b_dir.path().display().to_string()),
        }],
        ..Default::default()
    });

    let state = make_test_state_with_workflow_runtime_config_and_registry(
        repo_a_dir.path(),
        repo_a_dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/repo-b" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "planning");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_runtime_issue_submission(
        &state,
        repo_b_dir.path(),
        Some("org/repo-b"),
        77,
        json["task_id"].as_str().expect("task id should be present"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn webhook_routes_runtime_prompt_to_repo_specific_project_root() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let repo_a_dir = crate::test_helpers::tempdir_in_home("webhook-prompt-a-")?;
    let repo_b_dir = crate::test_helpers::tempdir_in_home("webhook-prompt-b-")?;
    let secret = "secret";

    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "org/repo-b".to_string(),
            label: "harness".to_string(),
            project_root: Some(repo_b_dir.path().display().to_string()),
        }],
        ..Default::default()
    });

    let state = make_test_state_with_workflow_runtime_config_and_registry(
        repo_a_dir.path(),
        repo_a_dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "created",
        "issue": {
            "number": 42,
            "html_url": "https://github.com/org/repo-b/pull/42",
            "pull_request": { "url": "https://api.github.com/repos/org/repo-b/pulls/42" }
        },
        "comment": {
            "body": "@harness fix ci",
            "html_url": "https://github.com/org/repo-b/issues/42#issuecomment-1"
        },
        "repository": { "full_name": "org/repo-b" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    let runtime_task_id = json["task_id"].as_str().expect("task id should be present");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_prompt_submission(&state, repo_b_dir.path(), runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_ignores_issue_tasks_when_repo_is_unmapped() -> anyhow::Result<()> {
    let repo_a_dir = crate::test_helpers::tempdir_in_home("webhook-fallback-a-")?;
    let repo_b_dir = crate::test_helpers::tempdir_in_home("webhook-fallback-b-")?;
    let secret = "secret";

    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "org/repo-b".to_string(),
            label: "harness".to_string(),
            project_root: Some(repo_b_dir.path().display().to_string()),
        }],
        ..Default::default()
    });

    let (state, _agent) =
        make_test_state_with_agent_and_config(repo_a_dir.path(), repo_a_dir.path(), config).await?;
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 78,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/unmapped" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "ignored");
    assert!(
        json["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not configured"),
        "reason should explain why the repo was ignored"
    );
    assert_eq!(state.core.tasks.count(), 0);
    Ok(())
}

#[tokio::test]
async fn webhook_pull_request_review_changes_requested_requests_local_review_gate(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "org/repo".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "submitted",
        "review": {
            "state": "changes_requested",
            "body": "Please fix the error handling.",
            "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-1"
        },
        "pull_request": {
            "number": 10,
            "html_url": "https://github.com/org/repo/pull/10"
        },
        "repository": { "full_name": "org/repo" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "pull_request_review")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "waiting");
    assert_eq!(json["workflow_state"], "local_review_gate");
    assert_eq!(json["execution_path"], "workflow_runtime");
    let runtime_task_id = json["task_id"].as_str().expect("task id should be present");
    assert_eq!(state.core.tasks.count(), before_count);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let instance = store
        .get_instance_by_task_id(runtime_task_id)
        .await?
        .expect("runtime local review workflow should be persisted");
    assert_runtime_local_review_requested(&state, &instance.id, runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_ping_event_returns_accepted_without_creating_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({ "zen": "Design for failure." });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "ping")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

// --- Real router (build_router) coverage ---
// The tests above use hand-built minimal routers. The three tests below use
// the real `http_router::build_router` so that dropped routes, missing
// DefaultBodyLimit wiring, or removed auth middleware fail CI rather than
// failing only after deploy.

#[tokio::test]
async fn build_router_health_route_returns_ok() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = super::http_router::build_router(state);

    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn build_router_auth_middleware_blocks_unauthenticated_requests() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("test-token-abc".to_string());
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = super::http_router::build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/tasks")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn build_router_webhook_body_limit_enforced() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let body_limit = state.core.server.config.server.max_webhook_body_bytes;
    let app = super::http_router::build_router(state);

    let oversized = vec![b'a'; body_limit + 1024];
    let signature = webhook_signature(secret, &oversized);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(oversized))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

// --- Startup recovery coverage ---
// spawn_pr_recovery and spawn_checkpoint_recovery only run at server restart,
// so CI never exercised them before these tests. A regression in the callback,
// permit, or project-resolution wiring would leave tasks stuck in pending
// forever in production while CI stayed green.
