use super::common::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn health_endpoint_returns_ok_and_task_count() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let health = call_health(state).await?;
    assert_eq!(health.status, "ok");
    assert_eq!(health.tasks, 0);
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert!(!health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_subsystem_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["q_value_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.degraded_subsystems, ["q_value_store"]);
    assert!(!health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_runtime_state_dirty() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    state.runtime_state_dirty.store(true, Ordering::Release);
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert!(health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_degraded_multiple_subsystems() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems =
        vec!["q_value_store", "runtime_state_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["q_value_store", "runtime_state_store"]
    );
    Ok(())
}

#[tokio::test]
async fn health_degraded_both_conditions() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["workspace_manager"];
    state.runtime_state_dirty.store(true, Ordering::Release);
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["workspace_manager"]
    );
    assert!(health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn token_usage_route_is_registered() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = token_usage_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/token-usage")
                .body(Body::empty())?,
        )
        .await?;

    assert_ne!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.status() == StatusCode::OK
            || response.status() == StatusCode::INTERNAL_SERVER_ERROR
    );
    Ok(())
}

#[tokio::test]
async fn create_task_with_prompt_returns_accepted() -> anyhow::Result<()> {
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

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    use http_body_util::BodyExt;
    let resp_body = response.into_body().collect().await?.to_bytes();
    let resp: serde_json::Value = serde_json::from_slice(&resp_body)?;
    assert!(resp["task_id"].is_string());
    assert_eq!(state.core.tasks.count(), before_count + 1);
    Ok(())
}

#[tokio::test]
async fn create_task_empty_request_returns_bad_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let app = task_app(state);

    let body = serde_json::json!({});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn get_task_returns_not_found_for_missing_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = task_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/nonexistent-id")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn create_then_get_task_returns_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;

    let create_body = serde_json::json!({ "prompt": "add tests" });
    let create_resp = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(create_body.to_string()))?,
        )
        .await?;
    assert_eq!(create_resp.status(), StatusCode::ACCEPTED);

    use http_body_util::BodyExt;
    let create_body = create_resp.into_body().collect().await?.to_bytes();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body)?;
    let task_id = create_json["task_id"]
        .as_str()
        .expect("task_id should be string");

    let get_resp = task_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{task_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_resp.status(), StatusCode::OK);

    let get_body = get_resp.into_body().collect().await?.to_bytes();
    let task_json: serde_json::Value = serde_json::from_slice(&get_body)?;
    assert_eq!(task_json["id"], task_id);
    Ok(())
}

#[tokio::test]
async fn api_tasks_alias_returns_ok() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = list_tasks_app(state);

    let response = app
        .oneshot(Request::builder().uri("/api/tasks").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn tasks_and_api_tasks_return_same_data() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    use http_body_util::BodyExt;

    let tasks_resp = list_tasks_app(state.clone())
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    let api_tasks_resp = list_tasks_app(state)
        .oneshot(Request::builder().uri("/api/tasks").body(Body::empty())?)
        .await?;

    assert_eq!(tasks_resp.status(), StatusCode::OK);
    assert_eq!(api_tasks_resp.status(), StatusCode::OK);

    let tasks_body: serde_json::Value =
        serde_json::from_slice(&tasks_resp.into_body().collect().await?.to_bytes())?;
    let api_tasks_body: serde_json::Value =
        serde_json::from_slice(&api_tasks_resp.into_body().collect().await?.to_bytes())?;
    assert_eq!(tasks_body, api_tasks_body);
    Ok(())
}

#[tokio::test]
async fn intake_status_returns_three_channels() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
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
    let state = make_test_state(dir.path()).await?;
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
    let state = make_test_state(dir.path()).await?;
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
    let state = make_test_state_with(
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
async fn intake_status_recent_dispatches_empty_initially() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
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
