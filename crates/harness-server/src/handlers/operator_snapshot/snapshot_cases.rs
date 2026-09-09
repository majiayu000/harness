use super::*;
use crate::test_helpers;
use axum::{body::to_bytes, routing::get, Router};
use harness_workflow::runtime::{
    WorkflowInstance, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID, PR_FEEDBACK_DEFINITION_ID,
};

fn runtime_workflow(state: &str, subject_key: &str, data: serde_json::Value) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("issue", subject_key),
    )
    .with_server_data(data)
}

async fn age_workflow(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE workflow_instances SET updated_at = NOW() - INTERVAL '2 hours' WHERE id = $1",
    )
    .bind(workflow_id)
    .execute(store.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn returns_200_with_all_top_level_keys() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    for key in [
        "generated_at",
        "retry",
        "rate_limits",
        "recent_failures",
        "runtime_logs",
    ] {
        assert!(body.get(key).is_some(), "missing top-level key: {key}");
    }
    assert!(body["retry"].get("last_tick").is_some());
    assert!(body["retry"].get("stalled_tasks").is_some());
    assert!(body["rate_limits"].get("signal_ingestion").is_some());
    assert!(body["rate_limits"].get("password_reset").is_some());
    assert_eq!(body["runtime_logs"]["state"], "disabled");
    Ok(())
}

#[tokio::test]
async fn last_tick_null_on_fresh_server() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-notick-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert!(
        body["retry"]["last_tick"].is_null(),
        "expected null last_tick on fresh server"
    );
    Ok(())
}

#[tokio::test]
async fn stalled_tasks_empty_on_fresh_server() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-nostall-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert_eq!(
        body["retry"]["stalled_tasks"].as_array().map(|a| a.len()),
        Some(0),
    );
    Ok(())
}

#[tokio::test]
async fn recent_failures_empty_on_fresh_server() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-nofail-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert_eq!(body["recent_failures"].as_array().map(|a| a.len()), Some(0),);
    Ok(())
}

#[test]
fn missing_timestamps_serialize_as_null() {
    let stalled_task = crate::task_runner::TaskState {
        id: harness_core::types::TaskId("stalled-task".to_string()),
        task_kind: crate::task_runner::TaskKind::Issue,
        status: crate::task_runner::TaskStatus::Implementing,
        failure_kind: None,
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: Some("issue:stalled".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(std::path::PathBuf::from("/test/stalled")),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: None,
        created_at: Some("2026-04-22T00:00:00Z".to_string()),
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    let stalled_json = stalled_task_json(&stalled_task);

    let failed_task = crate::task_runner::RecentFailureTask {
        id: harness_core::types::TaskId("failed-task".to_string()),
        failure_kind: Some(crate::task_runner::TaskFailureKind::Task),
        external_id: Some("issue:failed".to_string()),
        project: Some("/test/failed".to_string()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        error: Some("boom".to_string()),
        failed_at: None,
    };
    let failed_json = recent_failure_json(&failed_task);

    assert!(
        stalled_json["stalled_since"].is_null(),
        "expected stalled_since to serialize as null",
    );
    assert!(
        failed_json["failed_at"].is_null(),
        "expected failed_at to serialize as null",
    );
    assert!(
        failed_json["project"] == "failed",
        "expected recent failure project to serialize as basename only",
    );
}

#[tokio::test]
async fn malformed_retry_counters_fallback_to_zero() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-bad-tick-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let mut event = harness_core::types::Event::new(
        harness_core::types::SessionId::new(),
        "periodic_retry:summary",
        "retry_scheduler",
        harness_core::types::Decision::Pass,
    );
    event.detail = Some(r#"{"checked":"bad","retried":7,"stuck":null}"#.to_string());
    state.observability.events.log(&event).await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    let tick = &body["retry"]["last_tick"];
    assert_eq!(tick["checked"], 0);
    assert_eq!(tick["retried"], 7);
    assert_eq!(tick["stuck"], 0);
    assert_eq!(tick["skipped"], 0);
    Ok(())
}

#[tokio::test]
async fn last_tick_falls_back_to_older_summary_event() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-old-tick-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let mut event = harness_core::types::Event::new(
        harness_core::types::SessionId::new(),
        "periodic_retry:summary",
        "retry_scheduler",
        harness_core::types::Decision::Warn,
    );
    event.ts = Utc::now() - chrono::Duration::hours(3);
    event.detail = Some(r#"{"checked":1,"retried":0,"stuck":1,"skipped":0}"#.to_string());
    state.observability.events.log(&event).await?;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    let tick = &body["retry"]["last_tick"];
    assert_eq!(tick["checked"], 1);
    assert_eq!(tick["stuck"], 1);
    Ok(())
}

#[tokio::test]
async fn recent_failures_capped_at_max() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-cap-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    // Seed 25 failed tasks.
    for i in 0..25u32 {
        let mut task = crate::task_runner::TaskState {
            id: harness_core::types::TaskId(format!("fail-task-{i}")),
            task_kind: crate::task_runner::TaskKind::Issue,
            status: crate::task_runner::TaskStatus::Failed,
            failure_kind: Some(crate::task_runner::TaskFailureKind::Task),
            turn: 1,
            pr_url: None,
            rounds: vec![],
            error: Some(format!("error {i}")),
            source: None,
            external_id: Some(format!("issue:{i}")),
            parent_id: None,
            depends_on: vec![],
            subtask_ids: vec![],
            project_root: Some(std::path::PathBuf::from("/test/proj")),
            workspace_path: None,
            workspace_owner: None,
            run_generation: 0,
            issue: None,
            repo: None,
            description: None,
            created_at: None,
            updated_at: None,
            priority: 0,
            phase: crate::task_runner::TaskPhase::Implement,
            triage_output: None,
            plan_output: None,
            request_settings: None,
            scheduler: crate::task_runner::TaskSchedulerState::queued(),

            version: 0,
        };
        task.status = crate::task_runner::TaskStatus::Failed;
        state.core.tasks.insert(&task).await;
    }

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    let failures = body["recent_failures"].as_array().expect("array");
    assert!(
        failures.len() <= MAX_TASKS,
        "recent_failures should be capped at {MAX_TASKS}, got {}",
        failures.len()
    );
    Ok(())
}

#[tokio::test]
async fn long_error_is_truncated() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-trunc-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    let long_error = "x".repeat(500);
    let task = crate::task_runner::TaskState {
        id: harness_core::types::TaskId("trunc-task".to_string()),
        task_kind: crate::task_runner::TaskKind::Issue,
        status: crate::task_runner::TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::Task),
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: Some(long_error),
        source: None,
        external_id: Some("issue:trunc".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(std::path::PathBuf::from("/test/proj")),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    state.core.tasks.insert(&task).await;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    let failures = body["recent_failures"].as_array().expect("array");
    assert!(!failures.is_empty());
    let error_str = failures[0]["error"].as_str().expect("string");
    assert!(
        error_str.len() <= MAX_ERROR_LEN + 4, // +4 for the "…" suffix (multibyte)
        "error should be truncated, got len {}",
        error_str.len()
    );
    Ok(())
}

/// Regression test: multi-byte UTF-8 at the truncation boundary must not panic.
/// A 3-byte emoji repeated so that the cut falls mid-character verifies the
/// is_char_boundary walk-back in the truncation logic.
#[tokio::test]
async fn unicode_error_truncation_does_not_panic() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-unicode-")?;
    let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

    // "€" is 3 bytes; repeat enough times that the 200-byte cut falls mid-char.
    let unicode_error = "€".repeat(100); // 300 bytes total
    let task = crate::task_runner::TaskState {
        id: harness_core::types::TaskId("unicode-task".to_string()),
        task_kind: crate::task_runner::TaskKind::Issue,
        status: crate::task_runner::TaskStatus::Failed,
        failure_kind: Some(crate::task_runner::TaskFailureKind::Task),
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: Some(unicode_error),
        source: None,
        external_id: Some("issue:unicode".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(std::path::PathBuf::from("/test/proj")),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: crate::task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: crate::task_runner::TaskSchedulerState::queued(),

        version: 0,
    };
    state.core.tasks.insert(&task).await;

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);

    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    // Must not panic (no 500).
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;
    let failures = body["recent_failures"].as_array().expect("array");
    assert!(!failures.is_empty());
    let error_str = failures[0]["error"].as_str().expect("string");
    // Result must be valid UTF-8 (serde_json already guarantees this) and bounded.
    assert!(error_str.len() <= MAX_ERROR_LEN + 4);
    Ok(())
}

#[tokio::test]
async fn runtime_logs_returns_full_active_path_when_enabled() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-log-")?;
    let mut state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    let active_path = dir
        .path()
        .join("logs/harness-serve-20260430T120000Z-pid1.log");
    let expected_path = active_path.display().to_string();
    server.runtime_logs = crate::server::RuntimeLogMetadata::enabled(active_path, 30, 9);

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert_eq!(body["runtime_logs"]["state"], "enabled");
    assert_eq!(
        body["runtime_logs"]["active_path"],
        serde_json::Value::String(expected_path.clone())
    );
    assert_eq!(
        body["runtime_logs"]["path_hint"],
        serde_json::Value::String(expected_path)
    );
    assert_eq!(body["runtime_logs"]["retention_max_files"], 9);
    Ok(())
}

#[tokio::test]
async fn runtime_logs_reports_degraded_state_without_active_path() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-op-snap-runtime-log-degraded-")?;
    let mut state = Arc::new(test_helpers::make_test_state(dir.path()).await?);
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    server.runtime_logs = crate::server::RuntimeLogMetadata::degraded(
        Some("logs/harness-serve-20260430T120000Z-pid1.log".to_string()),
        30,
        2,
    );

    let app = Router::new()
        .route("/api/operator-snapshot", get(operator_snapshot))
        .with_state(state);
    let req = axum::http::Request::builder()
        .uri("/api/operator-snapshot")
        .body(axum::body::Body::empty())?;
    let resp = tower::ServiceExt::oneshot(app, req).await?;
    let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
    let body: serde_json::Value = serde_json::from_slice(&bytes)?;

    assert_eq!(body["runtime_logs"]["state"], "degraded");
    assert!(body["runtime_logs"]["active_path"].is_null());
    assert_eq!(
        body["runtime_logs"]["path_hint"],
        serde_json::Value::String("logs/harness-serve-20260430T120000Z-pid1.log".to_string())
    );
    assert_eq!(body["runtime_logs"]["retention_max_files"], 2);
    Ok(())
}
