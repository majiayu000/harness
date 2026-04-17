use super::*;

#[tokio::test]
async fn thread_start_persists_to_db() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
    let canonical_proj = proj_dir.path().canonicalize()?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadStart {
            cwd: proj_dir.path().to_path_buf(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    assert!(
        resp.error.is_none(),
        "expected success, got error: {:?}",
        resp.error
    );
    let thread_id_str = resp.result.unwrap()["thread_id"]
        .as_str()
        .unwrap()
        .to_string();

    let db = state.core.thread_db.as_ref().unwrap();
    let thread = db
        .get(&thread_id_str)
        .await?
        .expect("thread should be in DB");
    assert_eq!(thread.project_root, canonical_proj);
    Ok(())
}

#[tokio::test]
async fn event_log_then_query_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let session_id = harness_core::types::SessionId::new();
    let event = harness_core::types::Event::new(
        session_id.clone(),
        "pre_tool_use",
        "Edit",
        harness_core::types::Decision::Pass,
    );

    // Log the event via EventLog RPC
    let log_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::EventLog {
            event: event.clone(),
        },
    };
    let log_resp = handle_request(&state, log_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response for EventLog"))?;
    assert!(
        log_resp.error.is_none(),
        "EventLog should succeed: {:?}",
        log_resp.error
    );
    let result = log_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["logged"], serde_json::json!(true));
    let event_id = result["event_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing event_id"))?;
    assert_eq!(event_id, event.id.as_str());

    // Query the event via EventQuery RPC
    let query_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::EventQuery {
            filters: harness_core::types::EventFilters {
                session_id: Some(session_id),
                ..Default::default()
            },
        },
    };
    let query_resp = handle_request(&state, query_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response for EventQuery"))?;
    assert!(
        query_resp.error.is_none(),
        "EventQuery should succeed: {:?}",
        query_resp.error
    );
    let events_val = query_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let events_arr = events_val
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected JSON array"))?;
    assert_eq!(events_arr.len(), 1, "expected exactly one event");
    assert_eq!(
        events_arr[0]["id"],
        serde_json::json!(event.id.as_str()),
        "returned event id should match logged event"
    );
    Ok(())
}

// === Integration tests for previously unrouted methods ===

#[tokio::test]
async fn event_log_records_event() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let session_id = harness_core::types::SessionId::new();
    let event = harness_core::types::Event::new(
        session_id,
        "test_hook",
        "TestTool",
        harness_core::types::Decision::Pass,
    );

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::EventLog { event },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");

    assert!(
        resp.error.is_none(),
        "event_log should succeed: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["logged"], serde_json::json!(true));
    assert!(result["event_id"].is_string(), "event_id should be present");
    Ok(())
}

#[tokio::test]
async fn event_query_returns_logged_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let session_id = harness_core::types::SessionId::new();
    let event = harness_core::types::Event::new(
        session_id,
        "probe_hook",
        "ProbeTarget",
        harness_core::types::Decision::Pass,
    );
    state.observability.events.log(&event).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::EventQuery {
            filters: harness_core::types::EventFilters {
                hook: Some("probe_hook".to_string()),
                ..Default::default()
            },
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");

    assert!(
        resp.error.is_none(),
        "event_query should succeed: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let events = result
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array result"))?;
    assert_eq!(events.len(), 1, "should return exactly one matching event");
    Ok(())
}
