use super::runtime_hosts;
use axum::{
    body::Body,
    http::Request,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower::ServiceExt;

fn runtime_hosts_app(state: Arc<crate::http::AppState>) -> Router {
    Router::new()
        .route("/api/runtime-hosts", get(runtime_hosts::list_runtime_hosts))
        .route(
            "/api/runtime-hosts/register",
            post(runtime_hosts::register_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/heartbeat",
            post(runtime_hosts::heartbeat_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/deregister",
            post(runtime_hosts::deregister_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/tasks/claim",
            post(runtime_hosts::claim_task_for_runtime_host),
        )
        .with_state(state)
}

async fn make_test_state(
    dir: &std::path::Path,
) -> anyhow::Result<Option<Arc<crate::http::AppState>>> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(None);
    }
    match crate::test_helpers::make_test_state(dir).await {
        Ok(state) => Ok(Some(Arc::new(state))),
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

#[tokio::test]
async fn register_then_list_runtime_hosts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_app(state);

    let body = serde_json::json!({
        "host_id": "host-a",
        "display_name": "Host A",
        "capabilities": ["claude", "codex"]
    });
    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    let list = app
        .oneshot(
            Request::builder()
                .uri("/api/runtime-hosts")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(list.status(), axum::http::StatusCode::OK);
    let data = http_body_util::BodyExt::collect(list.into_body())
        .await?
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&data)?;
    assert_eq!(json["hosts"][0]["id"], "host-a");
    assert_eq!(json["hosts"][0]["online"], true);
    Ok(())
}

#[tokio::test]
async fn claim_endpoint_blocks_double_claim() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };
    let task = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        task_kind: crate::task_runner::TaskKind::Prompt,
        status: crate::task_runner::TaskStatus::Pending,
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
        description: Some("pending task".to_string()),
        created_at: Some("2026-04-02T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;
    let app = runtime_hosts_app(state.clone());

    for host in ["host-a", "host-b"] {
        let body = serde_json::json!({ "host_id": host });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/runtime-hosts/register")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))?,
            )
            .await?;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/tasks/claim")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "lease_secs": 30 }).to_string(),
                ))?,
        )
        .await?;
    let first_json: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(first.into_body())
            .await?
            .to_bytes(),
    )?;
    assert_eq!(first_json["claimed"], true);
    assert_eq!(first_json["task_id"], task_id.to_string());
    let claimed_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("claimed task should remain cached");
    assert_eq!(claimed_state.scheduler.runtime_host_id(), Some("host-a"));
    assert_eq!(claimed_state.scheduler.run_generation, 1);
    assert!(claimed_state.scheduler.lease_expires_at.is_some());

    let second = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-b/tasks/claim")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "lease_secs": 30 }).to_string(),
                ))?,
        )
        .await?;
    let second_json: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(second.into_body())
            .await?
            .to_bytes(),
    )?;
    assert_eq!(second_json["claimed"], false);
    Ok(())
}

#[tokio::test]
async fn deregister_releases_scheduler_owned_pending_tasks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
    let mut task = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        status: crate::task_runner::TaskStatus::Pending,
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
        task_kind: crate::task_runner::TaskKind::default(),
        description: Some("leased task".to_string()),
        created_at: Some("2026-04-02T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::seconds(60),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    let app = runtime_hosts_app(state.clone());

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "host_id": "host-a" }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let task = state
        .core
        .tasks
        .get(&task_id)
        .expect("task should still exist after deregistration");
    assert_eq!(task.scheduler.runtime_host_id(), None);
    assert!(matches!(
        task.scheduler.authority_state,
        crate::task_runner::SchedulerAuthorityState::Queued
    ));
    Ok(())
}

#[tokio::test]
async fn claim_endpoint_honors_project_filter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };

    let project_a = dir.path().join("project-a");
    let project_b = dir.path().join("project-b");

    let task_a = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        task_kind: crate::task_runner::TaskKind::Prompt,
        status: crate::task_runner::TaskStatus::Pending,
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
        project_root: Some(project_a),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: Some("pending task a".to_string()),
        created_at: Some("2026-04-02T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    let task_b = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        task_kind: crate::task_runner::TaskKind::Prompt,
        status: crate::task_runner::TaskStatus::Pending,
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
        project_root: Some(project_b.clone()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: Some("pending task b".to_string()),
        created_at: Some("2026-04-02T00:00:01Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    let task_b_id = task_b.id.clone();

    state.core.tasks.insert(&task_a).await;
    state.core.tasks.insert(&task_b).await;

    let app = runtime_hosts_app(state);

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "host_id": "host-a" }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/tasks/claim")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "lease_secs": 30,
                        "project": project_b.to_string_lossy(),
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(response.into_body())
            .await?
            .to_bytes(),
    )?;
    assert_eq!(json["claimed"], true);
    assert_eq!(json["task_id"], task_b_id.to_string());
    Ok(())
}

#[tokio::test]
async fn claim_endpoint_rejects_out_of_range_lease_secs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };
    let task = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        task_kind: crate::task_runner::TaskKind::Prompt,
        status: crate::task_runner::TaskStatus::Pending,
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
        description: Some("pending task".to_string()),
        created_at: Some("2026-04-02T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    state.core.tasks.insert(&task).await;
    let app = runtime_hosts_app(state);

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "host_id": "host-a" }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/tasks/claim")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "lease_secs": u64::MAX }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(response.into_body())
            .await?
            .to_bytes(),
    )?;
    assert_eq!(json["error"], "lease_secs must be <= i64::MAX");
    Ok(())
}

#[tokio::test]
async fn claim_endpoint_rejects_overflowing_lease_ttl() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };
    let task = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        task_kind: crate::task_runner::TaskKind::Prompt,
        status: crate::task_runner::TaskStatus::Pending,
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
        description: Some("pending task".to_string()),
        created_at: Some("2026-04-02T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    state.core.tasks.insert(&task).await;
    let app = runtime_hosts_app(state);

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "host_id": "host-a" }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/tasks/claim")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "lease_secs": i64::MAX as u64 }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let json: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(response.into_body())
            .await?
            .to_bytes(),
    )?;
    assert!(json["error"]
        .as_str()
        .unwrap_or_default()
        .contains("too large to compute a valid expiration timestamp"));
    Ok(())
}

#[tokio::test]
async fn deregister_keeps_host_registered_when_claim_release_fails() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
    let mut task = crate::task_runner::TaskState {
        id: crate::task_runner::TaskId::new(),
        status: crate::task_runner::TaskStatus::Pending,
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
        task_kind: crate::task_runner::TaskKind::default(),
        description: Some("leased task".to_string()),
        created_at: Some("2026-04-02T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),
    };
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::seconds(60),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    let app = runtime_hosts_app(state.clone());

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "host_id": "host-a" }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    crate::test_helpers::drop_tasks_table(dir.path()).await?;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(state.runtime_hosts.hosts.contains_key("host-a"));

    let task = state
        .core
        .tasks
        .get(&task_id)
        .expect("task should remain cached after failed deregistration");
    assert_eq!(task.scheduler.runtime_host_id(), Some("host-a"));
    assert!(matches!(
        task.scheduler.authority_state,
        crate::task_runner::SchedulerAuthorityState::Leased
    ));
    Ok(())
}
