use super::*;

struct DummyGithubPoller;

#[async_trait]
impl crate::intake::IntakeSource for DummyGithubPoller {
    fn name(&self) -> &str {
        "github"
    }

    async fn poll(&self) -> anyhow::Result<Vec<crate::intake::IncomingIssue>> {
        Ok(Vec::new())
    }

    async fn mark_dispatched(
        &self,
        _external_id: &str,
        _task_id: &task_runner::TaskId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn unmark_dispatched(&self, _external_id: &str) {}

    async fn on_task_complete(
        &self,
        _external_id: &str,
        _result: &crate::intake::TaskCompletionResult,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn intake_status_returns_three_channels() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;

    let channels = json["channels"].as_array().expect("channels is array");
    assert_eq!(channels.len(), 3);
    let names: Vec<&str> = channels
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"github"));
    assert!(names.contains(&"feishu"));
    assert!(names.contains(&"dashboard"));
    Ok(())
}

#[tokio::test]
async fn intake_status_github_disabled_by_default() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["enabled"], false);
    Ok(())
}

#[tokio::test]
async fn intake_status_dashboard_always_enabled() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let dashboard = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "dashboard")
        .expect("dashboard channel present");
    assert_eq!(dashboard["enabled"], true);
    Ok(())
}

#[tokio::test]
async fn intake_status_shows_github_repo_when_configured() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "owner/myrepo".to_string(),
        label: "harness".to_string(),
        poll_interval_secs: 30,
        ..Default::default()
    });
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["enabled"], true);
    assert_eq!(github["repo"], "owner/myrepo");
    Ok(())
}

#[tokio::test]
async fn intake_status_reports_github_mode_drivers_and_effective_repos() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some("secret".to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        mode: harness_core::config::intake::IntakeMode::Hybrid,
        repo: "owner/main".to_string(),
        label: "harness".to_string(),
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/secondary".to_string(),
            label: "bugs".to_string(),
            project_root: Some("/tmp/secondary".to_string()),
            auto_merge: None,
            auto_recovery: None,
            merge_method: None,
            delete_branch: None,
            require_review_threads_resolved: None,
            require_clean_merge_state: None,
        }],
        ..Default::default()
    });
    let mut state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    state_mut
        .intake
        .github_pollers
        .push(Arc::new(DummyGithubPoller));
    state_mut
        .intake
        .github_poller_repos
        .push("owner/main".to_string());
    let app = intake_app(state);

    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["enabled"], true);
    assert_eq!(github["repo"], "owner/main");
    assert_eq!(github["mode"], "hybrid");
    assert_eq!(github["drivers"]["webhook"]["configured"], true);
    assert_eq!(github["drivers"]["webhook"]["accepting"], true);
    assert_eq!(github["drivers"]["webhook"]["degraded"], false);
    assert_eq!(github["drivers"]["polling"]["configured"], true);
    assert_eq!(github["drivers"]["polling"]["active"], true);
    assert_eq!(
        github["drivers"]["polling"]["discovery_driver"],
        "direct_rest"
    );
    assert_eq!(
        github["drivers"]["polling"]["reason"],
        serde_json::Value::Null
    );
    let repos = github["repos"]
        .as_array()
        .expect("repos should be an array");
    assert!(repos.iter().any(|repo| repo["repo"] == "owner/main"
        && repo["mode"] == "hybrid"
        && repo["drivers"]["discovery_driver"] == "direct_rest"));
    assert!(repos.iter().any(|repo| {
        repo["repo"] == "owner/secondary"
            && repo["label"] == "bugs"
            && repo["project_root"] == "/tmp/secondary"
    }));
    Ok(())
}

#[tokio::test]
async fn intake_status_reports_webhook_driver_degraded_without_secret() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        mode: harness_core::config::intake::IntakeMode::Webhook,
        repo: "owner/webhook".to_string(),
        ..Default::default()
    });
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = intake_app(state);

    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["mode"], "webhook");
    assert_eq!(github["drivers"]["webhook"]["configured"], true);
    assert_eq!(github["drivers"]["webhook"]["accepting"], false);
    assert_eq!(github["drivers"]["webhook"]["degraded"], true);
    assert_eq!(
        github["drivers"]["webhook"]["reason"],
        "missing_webhook_secret"
    );
    assert_eq!(github["drivers"]["polling"]["configured"], false);
    assert_eq!(json["degraded"]["partial"], true);
    assert_eq!(
        json["degraded"]["missing"],
        serde_json::json!([crate::http::github_intake_status::GITHUB_WEBHOOK_INTAKE_SUBSYSTEM])
    );
    assert_eq!(
        json["degraded"]["reason"],
        "github_webhook_secret_unavailable"
    );
    Ok(())
}

#[tokio::test]
async fn intake_status_recent_dispatches_empty_initially() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(json["recent_dispatches"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn intake_status_includes_runtime_github_issue_dispatches() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "owner/repo".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &project_root,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = task_runner::TaskId::from_str("runtime-github-intake-status");
    crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 65,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:65"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["active"], 1);
    let dispatch = json["recent_dispatches"]
        .as_array()
        .expect("recent dispatches should be an array")
        .iter()
        .find(|dispatch| dispatch["task_id"] == "runtime-github-intake-status")
        .expect("runtime GitHub issue dispatch should be listed");
    assert_eq!(dispatch["source"], "github");
    assert_eq!(dispatch["external_id"], "issue:65");
    assert_eq!(dispatch["tracker_source"], "github");
    assert_eq!(dispatch["tracker_external_id"], "issue:65");
    Ok(())
}

#[tokio::test]
async fn intake_status_merges_runtime_dispatches_by_recency_before_limit() -> anyhow::Result<()> {
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
    let old_created_at = Utc::now() - chrono::Duration::hours(2);
    for index in 0..10 {
        let mut task = task_runner::TaskState::new(task_runner::TaskId::from_str(&format!(
            "legacy-github-intake-{index}"
        )));
        task.source = Some("github".to_string());
        task.external_id = Some(format!("issue:{index}"));
        task.created_at = Some((old_created_at - chrono::Duration::minutes(index)).to_rfc3339());
        state.core.tasks.insert(&task).await;
    }
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = task_runner::TaskId::from_str("runtime-newest-intake-status");
    crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 165,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: Some("github"),
            external_id: Some("issue:165"),
            remote_fact_hash: None,
            author_trust_class: None,
        },
    )
    .await?;
    let app = intake_app(state);

    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    let dispatches = json["recent_dispatches"]
        .as_array()
        .expect("recent dispatches should be an array");
    assert_eq!(dispatches.len(), 10);
    assert_eq!(dispatches[0]["task_id"], "runtime-newest-intake-status");
    assert!(
        dispatches
            .iter()
            .any(|dispatch| dispatch["task_id"] == "runtime-newest-intake-status"),
        "newer runtime dispatch should not be truncated by older legacy rows"
    );
    Ok(())
}

#[tokio::test]
async fn intake_status_marks_runtime_submissions_degraded_when_store_unavailable(
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
    let app = intake_app(state);

    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    assert_eq!(json["degraded"]["partial"], true);
    assert_eq!(
        json["degraded"]["missing"],
        serde_json::json!(["workflow_runtime_submissions"])
    );
    assert_eq!(
        json["degraded"]["reason"],
        "runtime_submission_summaries_unavailable"
    );
    Ok(())
}

#[tokio::test]
async fn intake_status_disables_feishu_when_verification_token_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), None).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let feishu = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "feishu")
        .expect("feishu channel present");
    assert_eq!(feishu["enabled"], false);
    assert_eq!(feishu["keyword"], "harness");
    Ok(())
}

/// Build a minimal router that includes the auth middleware, mirroring how the
/// real server wires up the dashboard, tasks, and runtime operator endpoints.
fn authed_app(state: Arc<AppState>) -> Router {
    use axum::middleware;
    Router::new()
        .route("/", get(crate::dashboard::index))
        .route("/dashboard", get(crate::dashboard::index))
        .route("/health", get(health_check))
        .route(
            "/api/workflows/runtime/submissions",
            get(task_query_routes::list_runtime_submissions),
        )
        .route(
            "/api/workflows/runtime/unblock",
            post(task_mutation_routes::unblock_workflow_runtime),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state)
}

/// / and /dashboard are exempt from auth because dashboard HTML embeds no secrets.
#[tokio::test]
async fn dashboard_exempt_from_auth_when_token_configured() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = authed_app(state);

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dashboard?tab=submit")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

/// Verify that query-param token no longer grants access to protected endpoints.
#[tokio::test]
async fn query_param_token_rejected_on_protected_endpoint() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = authed_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/unblock?token=secret123")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn query_param_token_authorizes_runtime_sse_stream_endpoint() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/submissions/{id}/stream",
            get(|| async { StatusCode::OK }),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state);

    let path = "/api/workflows/runtime/submissions/task-1/stream?token=secret123";
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn dashboard_no_auth_configured_remains_public() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = authed_app(state);

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dashboard?tab=submit")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn protected_route_passes_with_explicit_unauthenticated_opt_in() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.allow_unauthenticated = true;
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = authed_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn list_tasks_rejects_running_as_task_status() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/submissions",
            get(task_query_routes::list_runtime_submissions),
        )
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions?status=running")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "invalid_status");
    assert_eq!(
        body["hint"],
        "Use scheduler_state=running instead of status=running."
    );
    Ok(())
}

#[tokio::test]
async fn list_tasks_rejects_invalid_limit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/submissions",
            get(task_query_routes::list_runtime_submissions),
        )
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/submissions?limit=0")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "invalid_limit");
    Ok(())
}
