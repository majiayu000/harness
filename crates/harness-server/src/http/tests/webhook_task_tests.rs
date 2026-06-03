use super::*;

#[tokio::test]
async fn webhook_issue_mention_schedules_runtime_issue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "majiayu000/harness".to_string(),
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
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
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
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_issue_submission(
        &state,
        dir.path(),
        Some("majiayu000/harness"),
        106,
        json["task_id"].as_str().expect("task id should be present"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn webhook_review_on_pr_requests_runtime_pr_feedback() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "majiayu000/harness".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let (workflow_id, runtime_task_id) =
        seed_bound_runtime_pr_workflow(&state, dir.path(), "majiayu000/harness", 42, 42).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 42, "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" } },
        "comment": { "body": "@harness review" },
        "repository": { "full_name": "majiayu000/harness" }
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
    assert_eq!(json["status"], "awaiting_feedback");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_eq!(json["task_id"], runtime_task_id);
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_local_review_requested(&state, &workflow_id, &runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_fix_ci_on_pr_creates_runtime_prompt_submission() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "majiayu000/harness".to_string(),
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
        "action": "created",
        "issue": {
            "number": 42,
            "html_url": "https://github.com/majiayu000/harness/pull/42",
            "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" }
        },
        "comment": {
            "body": "@harness fix CI",
            "html_url": "https://github.com/majiayu000/harness/issues/42#issuecomment-1"
        },
        "repository": { "full_name": "majiayu000/harness" }
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
    assert_runtime_prompt_submission(&state, dir.path(), runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_secret_requires_signature_header() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn webhook_secret_rejects_invalid_signature_value() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header(
                    "x-hub-signature-256",
                    "sha256=0000000000000000000000000000000000000000000000000000000000000000",
                )
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn webhook_empty_secret_configuration_fails_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("")).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

#[tokio::test]
async fn webhook_missing_secret_configuration_fails_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), None).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

#[tokio::test]
async fn webhook_rejects_invalid_event_header() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "Issue-Comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn webhook_body_limit_rejects_large_payload() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let body_limit = state.core.server.config.server.max_webhook_body_bytes;
    let app = webhook_app(state);

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

#[tokio::test]
async fn create_task_with_prompt_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let before_count = state.core.tasks.count();
    let app = task_app(state.clone());

    let body = serde_json::json!({ "prompt": "fix the bug" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = response_json(response).await?;
    assert!(
        resp["error"].as_str().is_some_and(
            |error| error.contains("workflow runtime store is required for prompt submissions")
        ),
        "unexpected response: {resp}"
    );
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn create_task_with_prompt_returns_workflow_runtime_submission() -> anyhow::Result<()> {
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
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "fix the prompt-only bug",
        "source": "dashboard",
        "external_id": "manual:prompt:http"
    });
    let response = app
        .clone()
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
    assert_eq!(resp["status"], "implementing");
    assert_eq!(resp["execution_path"], "workflow_runtime");
    let task_id = task_runner::TaskId::from_str(resp["task_id"].as_str().unwrap());
    assert!(state
        .core
        .tasks
        .get_with_db_fallback(&task_id)
        .await?
        .is_none());
    assert_eq!(state.core.tasks.count(), before_count);

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = crate::workflow_runtime_submission::prompt_workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("manual:prompt:http"),
        &task_id,
    );
    assert_eq!(resp["workflow_id"], workflow_id);
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.definition_id, "prompt_task");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert!(instance.data.get("prompt").is_none());
    assert_eq!(instance.data["prompt_summary"], "prompt task");
    assert_eq!(
        instance.data["prompt_chars"],
        "fix the prompt-only bug".chars().count()
    );
    let prompt_ref = instance.data["prompt_ref"]
        .as_str()
        .expect("prompt ref should be persisted");
    assert_eq!(
        crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref).as_deref(),
        Some("fix the prompt-only bug")
    );
    assert_eq!(instance.data["source"], "dashboard");
    assert_eq!(instance.data["external_id"], "manual:prompt:http");
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    assert!(commands[0].command.command.get("prompt").is_none());
    assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);

    let detail_response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail = response_json(detail_response).await?;
    assert_eq!(detail["task_kind"], "prompt");
    assert_eq!(detail["status"], "implementing");
    assert_eq!(detail["execution_path"], "workflow_runtime");
    assert_eq!(detail["workflow_id"], workflow_id);
    assert_eq!(detail["workflow"]["id"], workflow_id);
    assert_eq!(detail["workflow"]["definition_id"], "prompt_task");
    assert_eq!(detail["workflow"]["state"], "implementing");
    assert_eq!(
        detail["workflow"]["project_id"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert!(detail["workflow"].get("data").is_none());
    Ok(())
}

#[tokio::test]
async fn create_task_with_issue_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let before_count = state.core.tasks.count();
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "repo": "owner/repo",
        "issue": 42,
        "labels": ["bug"]
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

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = response_json(response).await?;
    assert!(
        resp["error"].as_str().is_some_and(|error| error
            .contains("workflow runtime store is required for GitHub issue submissions")),
        "unexpected response: {resp}"
    );
    assert_eq!(state.core.tasks.count(), before_count);

    Ok(())
}

#[tokio::test]
async fn create_task_with_issue_returns_workflow_runtime_submission() -> anyhow::Result<()> {
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
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "repo": "owner/repo",
        "issue": 42,
        "labels": ["bug"]
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
    assert_eq!(resp["status"], "implementing");
    assert_eq!(resp["execution_path"], "workflow_runtime");
    let task_id = task_runner::TaskId::from_str(resp["task_id"].as_str().unwrap());
    assert!(state
        .core
        .tasks
        .get_with_db_fallback(&task_id)
        .await?
        .is_none());
    assert_eq!(state.core.tasks.count(), before_count);

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        42,
    );
    assert_eq!(resp["workflow_id"], workflow_id);
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
    Ok(())
}
