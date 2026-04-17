use super::*;

#[tokio::test]
async fn initialized_returns_success() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        // `initialized` is typically a notification, but we allow an `id` for
        // compatibility and return an empty success response in that case.
        id: Some(serde_json::json!(1)),
        method: Method::Initialized,
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    assert!(
        resp.error.is_none(),
        "initialized should succeed, got error: {:?}",
        resp.error
    );
    assert!(resp.result.is_some(), "initialized must return a result");
    Ok(())
}

#[tokio::test]
async fn initialize_then_initialized_succeeds() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    // Start uninitialised to test the full handshake.
    state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Step 1: initialize
    let init_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::Initialize,
    };
    let init_resp = handle_request(&state, init_req)
        .await
        .expect("expected response for initialize");
    assert!(
        init_resp.error.is_none(),
        "initialize should succeed: {:?}",
        init_resp.error
    );
    let result = init_resp
        .result
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("initialize must return result"))?;
    assert!(
        result["capabilities"].is_object(),
        "capabilities should be present"
    );

    // Step 2: initialized (notification — id is None)
    let ack_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: None,
        method: Method::Initialized,
    };
    let ack_resp = handle_request(&state, ack_req).await;
    assert!(
        ack_resp.is_none(),
        "expected no response for initialized notification, got: {ack_resp:?}"
    );
    Ok(())
}

#[tokio::test]
async fn pre_init_request_rejected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadList,
    };
    let resp = handle_request(&state, req)
        .await
        .expect("should return error response");
    assert!(resp.error.is_some(), "pre-init request should be rejected");
    assert_eq!(
        resp.error.unwrap().code,
        harness_protocol::methods::NOT_INITIALIZED
    );
    Ok(())
}

#[tokio::test]
async fn double_initialize_rejected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    // state.notifications.initialized is already true from make_test_state

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::Initialize,
    };
    let resp = handle_request(&state, req)
        .await
        .expect("should return error response");
    assert!(resp.error.is_some(), "double init should be rejected");
    Ok(())
}

#[tokio::test]
async fn initialized_without_initialize_rejected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    // Simulate a fresh server where the handshake has not started.
    state.notifications.initializing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::Initialized,
    };
    let resp = handle_request(&state, req)
        .await
        .expect("should return error response");
    assert!(
        resp.error.is_some(),
        "initialized without initialize should be rejected"
    );
    assert_eq!(
        resp.error.unwrap().code,
        harness_protocol::methods::INVALID_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn handshake_unlocks_methods() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Before handshake: ThreadList rejected
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadList,
    };
    let resp = handle_request(&state, req).await.unwrap();
    assert!(resp.error.is_some());

    // Initialize
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::Initialize,
    };
    let resp = handle_request(&state, req).await.unwrap();
    assert!(resp.error.is_none(), "initialize should succeed");

    // Initialized (complete handshake)
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: None,
        method: Method::Initialized,
    };
    handle_request(&state, req).await;

    // After handshake: ThreadList works
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(3)),
        method: Method::ThreadList,
    };
    let resp = handle_request(&state, req).await.unwrap();
    assert!(resp.error.is_none(), "post-init request should work");
    Ok(())
}
