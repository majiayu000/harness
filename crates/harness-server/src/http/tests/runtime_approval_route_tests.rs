use super::*;
use harness_core::agent::{AgentAdapter, AgentEvent, ApprovalDecision, TurnRequest};
use harness_core::types::AgentId;
use std::sync::atomic::Ordering;

struct ApprovalTrackingAdapter {
    called: Arc<AtomicBool>,
    received: Arc<Mutex<Option<(String, ApprovalDecision)>>>,
}

#[async_trait]
impl AgentAdapter for ApprovalTrackingAdapter {
    fn name(&self) -> &str {
        "approval-tracking"
    }

    async fn start_turn(
        &self,
        _request: TurnRequest,
        _events: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        Ok(())
    }

    async fn respond_approval(
        &self,
        request_id: String,
        decision: ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        self.called.store(true, Ordering::SeqCst);
        *self.received.lock().await = Some((request_id, decision));
        Ok(())
    }
}

#[tokio::test]
async fn runtime_approval_route_forwards_decision_to_live_adapter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let thread_manager = crate::thread_manager::ThreadManager::new();
    let thread_id = thread_manager.start_thread(dir.path().to_path_buf());
    let turn_id =
        thread_manager.start_turn(&thread_id, "test approval".to_string(), AgentId::new())?;
    let called = Arc::new(AtomicBool::new(false));
    let received = Arc::new(Mutex::new(None));
    thread_manager.register_active_adapter(
        &turn_id,
        Arc::new(ApprovalTrackingAdapter {
            called: called.clone(),
            received: received.clone(),
        }),
    );

    let response = runtime_submission_routes::respond_to_approval_with_manager(
        &thread_manager,
        turn_id.to_string(),
        "request-1".to_string(),
        ApprovalDecision::Accept,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(
        *received.lock().await,
        Some(("request-1".to_string(), ApprovalDecision::Accept))
    );
    Ok(())
}

#[tokio::test]
async fn runtime_approval_route_trims_turn_and_request_ids() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let thread_manager = crate::thread_manager::ThreadManager::new();
    let thread_id = thread_manager.start_thread(dir.path().to_path_buf());
    let turn_id =
        thread_manager.start_turn(&thread_id, "test approval".to_string(), AgentId::new())?;
    let called = Arc::new(AtomicBool::new(false));
    let received = Arc::new(Mutex::new(None));
    thread_manager.register_active_adapter(
        &turn_id,
        Arc::new(ApprovalTrackingAdapter {
            called: called.clone(),
            received: received.clone(),
        }),
    );

    let response = runtime_submission_routes::respond_to_approval_with_manager(
        &thread_manager,
        format!("  {turn_id}  "),
        "  request-1  ".to_string(),
        ApprovalDecision::Accept,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(
        *received.lock().await,
        Some(("request-1".to_string(), ApprovalDecision::Accept))
    );
    Ok(())
}

#[tokio::test]
async fn runtime_approval_route_resolves_submission_handle_to_live_turn() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let thread_manager = crate::thread_manager::ThreadManager::new();
    let thread_id = thread_manager.start_thread(dir.path().to_path_buf());
    let turn_id =
        thread_manager.start_turn(&thread_id, "test approval".to_string(), AgentId::new())?;
    thread_manager.add_item(
        &thread_id,
        &turn_id,
        harness_core::types::Item::ApprovalRequest {
            id: Some("request-1".to_string()),
            action: "run tests".to_string(),
            approved: None,
        },
    )?;
    let called = Arc::new(AtomicBool::new(false));
    let received = Arc::new(Mutex::new(None));
    thread_manager.register_active_adapter(
        &turn_id,
        Arc::new(ApprovalTrackingAdapter {
            called: called.clone(),
            received: received.clone(),
        }),
    );
    thread_manager.register_runtime_turn_alias("submission-1", &turn_id);

    let response = runtime_submission_routes::respond_to_approval_with_manager(
        &thread_manager,
        "submission-1".to_string(),
        "request-1".to_string(),
        ApprovalDecision::Accept,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(
        *received.lock().await,
        Some(("request-1".to_string(), ApprovalDecision::Accept))
    );
    Ok(())
}

#[tokio::test]
async fn runtime_approval_route_is_protected_by_api_auth() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
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
            "/api/workflows/runtime/turns/{turn_id}/approvals/{request_id}",
            post(runtime_submission_routes::respond_to_approval),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/turns/turn-1/approvals/request-1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"decision":"accept"}"#))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn runtime_approval_route_accepts_tagged_decision_payload() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_read_only_route_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let thread_manager = &state.core.server.thread_manager;
    let thread_id = thread_manager.start_thread(dir.path().to_path_buf());
    let turn_id =
        thread_manager.start_turn(&thread_id, "test approval".to_string(), AgentId::new())?;
    let called = Arc::new(AtomicBool::new(false));
    let received = Arc::new(Mutex::new(None));
    thread_manager.register_active_adapter(
        &turn_id,
        Arc::new(ApprovalTrackingAdapter {
            called: called.clone(),
            received: received.clone(),
        }),
    );
    let app = Router::new()
        .route(
            "/api/workflows/runtime/turns/{turn_id}/approvals/{request_id}",
            post(runtime_submission_routes::respond_to_approval),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/workflows/runtime/turns/{turn_id}/approvals/request-1"
                ))
                .header("authorization", "Bearer secret123")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"decision":"accept"}"#))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(called.load(Ordering::SeqCst));
    assert_eq!(
        *received.lock().await,
        Some(("request-1".to_string(), ApprovalDecision::Accept))
    );
    Ok(())
}

#[tokio::test]
async fn runtime_approval_route_returns_not_found_for_unknown_turn() {
    let thread_manager = crate::thread_manager::ThreadManager::new();

    let response = runtime_submission_routes::respond_to_approval_with_manager(
        &thread_manager,
        "missing-turn".to_string(),
        "request-1".to_string(),
        ApprovalDecision::Accept,
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runtime_approval_route_rejects_empty_request_id() {
    let thread_manager = crate::thread_manager::ThreadManager::new();

    let response = runtime_submission_routes::respond_to_approval_with_manager(
        &thread_manager,
        "turn-1".to_string(),
        " ".to_string(),
        ApprovalDecision::Accept,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
