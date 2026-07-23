use super::*;

static CONTEXT_RUN_ID_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ContextRunIdEnvGuard {
    original_run_id: Option<String>,
    original_parent: Option<String>,
}

impl ContextRunIdEnvGuard {
    fn set(run_id: &str) -> Self {
        use harness_core::run_id::{AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};

        let original_run_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let original_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        // SAFETY: context tests serialize access with CONTEXT_RUN_ID_ENV_LOCK
        // and this guard restores both variables on drop.
        unsafe {
            std::env::set_var(AGENT_RUN_ID_ENV, run_id);
            std::env::remove_var(AGENT_RUN_PARENT_ENV);
        }
        Self {
            original_run_id,
            original_parent,
        }
    }
}

impl Drop for ContextRunIdEnvGuard {
    fn drop(&mut self) {
        use harness_core::run_id::{AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};

        // SAFETY: the guard is dropped while CONTEXT_RUN_ID_ENV_LOCK is held.
        unsafe {
            match self.original_run_id.take() {
                Some(value) => std::env::set_var(AGENT_RUN_ID_ENV, value),
                None => std::env::remove_var(AGENT_RUN_ID_ENV),
            }
            match self.original_parent.take() {
                Some(value) => std::env::set_var(AGENT_RUN_PARENT_ENV, value),
                None => std::env::remove_var(AGENT_RUN_PARENT_ENV),
            }
        }
    }
}

#[tokio::test]
async fn rule_check_returns_warning_when_no_guards_loaded() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-test-")?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::RuleCheck {
            project_root: proj_dir.path().to_path_buf(),
            files: None,
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    let error = resp
        .error
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("expected warning error response"))?;
    assert_eq!(error.code, VALIDATION_ERROR);
    assert!(
        error
            .message
            .contains(harness_rules::engine::WARN_NO_GUARDS_REGISTERED),
        "expected explicit warning message, got: {}",
        error.message
    );
    assert!(
        resp.result.is_none(),
        "warning path should not return a success payload"
    );

    let events = state
        .observability
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("rule_scan".to_string()),
            ..Default::default()
        })
        .await?;
    assert!(
        events.is_empty(),
        "no scan event should be logged when scan request is rejected"
    );
    Ok(())
}

#[tokio::test]
async fn metrics_query_counts_rule_violations_from_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let session_id = harness_core::types::SessionId::new();
    // The metrics_query handler scopes violation counts by the latest
    // rule_scan session, so we must log a rule_scan summary event first.
    let scan_event = harness_core::types::Event::new(
        session_id.clone(),
        "rule_scan",
        "RuleEngine",
        harness_core::types::Decision::Block,
    );
    state.observability.events.log(&scan_event).await?;
    for _ in 0..5 {
        let event = harness_core::types::Event::new(
            session_id.clone(),
            "rule_check",
            "SEC-01",
            harness_core::types::Decision::Block,
        );
        state.observability.events.log(&event).await?;
    }

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::MetricsQuery {
            filters: harness_core::types::MetricFilters::default(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let coverage = result["dimensions"]["coverage"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing coverage"))?;
    assert!(
        coverage < 100.0,
        "coverage should degrade with violations, got {coverage}"
    );
    Ok(())
}

#[tokio::test]
async fn metrics_query_sees_violations_written_via_handler_entry() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let violations = vec![
        harness_core::types::Violation {
            rule_id: harness_core::types::RuleId::from_str("SEC-01"),
            file: std::path::PathBuf::from("src/lib.rs"),
            line: Some(7),
            message: "critical issue".to_string(),
            severity: harness_core::types::Severity::Critical,
        },
        harness_core::types::Violation {
            rule_id: harness_core::types::RuleId::from_str("U-01"),
            file: std::path::PathBuf::from("src/main.rs"),
            line: None,
            message: "style issue".to_string(),
            severity: harness_core::types::Severity::Low,
        },
    ];
    state
        .observability
        .events
        .persist_rule_scan(&project_root, &violations)
        .await;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::MetricsQuery {
            filters: harness_core::types::MetricFilters::default(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let coverage = result["dimensions"]["coverage"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing coverage"))?;
    assert!(
        coverage < 100.0,
        "coverage should degrade with persisted violations, got {coverage}"
    );

    let events = state
        .observability
        .events
        .query(&harness_core::types::EventFilters::default())
        .await?;
    let latest_scan = events
        .iter()
        .rev()
        .find(|event| event.hook == "rule_scan")
        .ok_or_else(|| anyhow::anyhow!("missing rule_scan anchor event"))?;
    let linked_checks = events
        .iter()
        .filter(|event| event.hook == "rule_check" && event.session_id == latest_scan.session_id)
        .count();
    assert_eq!(linked_checks, violations.len());

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
            event: Box::new(event.clone()),
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

#[tokio::test]
async fn usage_probe_report_is_queryable_from_event_detail() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let before = harness_core::usage_probe::snapshot()
        .into_iter()
        .find(|entry| entry.surface == "task_runner")
        .map(|entry| entry.count)
        .unwrap_or(0);
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    harness_core::usage_probe::record_usage(
        harness_core::usage_probe::UsageProbeSurface::TaskRunner,
    );
    let event = harness_core::usage_probe::build_probe_report_event()?;
    state.observability.events.log(&event).await?;

    let events = state
        .observability
        .events
        .query(&harness_core::types::EventFilters {
            hook: Some("probe_report".to_string()),
            tool: Some("usage_probe".to_string()),
            ..Default::default()
        })
        .await?;

    assert_eq!(events.len(), 1);
    let detail = events[0]
        .detail
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing probe detail"))?;
    let counts: Vec<serde_json::Value> = serde_json::from_str(detail)?;
    let task_runner_count = counts
        .iter()
        .find(|entry| entry["surface"] == "task_runner")
        .and_then(|entry| entry["count"].as_u64())
        .unwrap_or(0);
    assert!(task_runner_count > before);
    assert!(events[0].content.is_none());
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
        method: Method::GcStatus,
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

    // Before handshake: GcStatus rejected
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::GcStatus,
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

    // After handshake: GcStatus works
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(3)),
        method: Method::GcStatus,
    };
    let resp = handle_request(&state, req).await.unwrap();
    assert!(resp.error.is_none(), "post-init request should work");
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
        method: Method::EventLog {
            event: Box::new(event),
        },
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

#[tokio::test]
async fn skill_create_and_get() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let create_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::SkillCreate {
            name: "test-skill".to_string(),
            content: "# Test Skill\nDoes things.".to_string(),
        },
    };
    let create_resp = handle_request(&state, create_req)
        .await
        .expect("expected response");
    assert!(
        create_resp.error.is_none(),
        "skill_create should succeed: {:?}",
        create_resp.error
    );
    let skill = create_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let skill_id_str = skill["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing id"))?
        .to_string();
    let skill_id = harness_core::types::SkillId::from_str(&skill_id_str);

    let get_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::SkillGet { skill_id },
    };
    let get_resp = handle_request(&state, get_req)
        .await
        .expect("expected response");
    assert!(
        get_resp.error.is_none(),
        "skill_get should succeed: {:?}",
        get_resp.error
    );
    let retrieved = get_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(retrieved["name"], serde_json::json!("test-skill"));
    Ok(())
}

#[tokio::test]
async fn skill_list_returns_created_skills() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    for name in ["alpha-skill", "beta-skill"] {
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::SkillCreate {
                name: name.to_string(),
                content: format!("# {name}\nDoes things."),
            },
        };
        let resp = handle_request(&state, req).await.expect("response");
        assert!(resp.error.is_none(), "skill_create should succeed");
    }

    let list_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::SkillList { query: None },
    };
    let list_resp = handle_request(&state, list_req)
        .await
        .expect("expected response");
    assert!(
        list_resp.error.is_none(),
        "skill_list should succeed: {:?}",
        list_resp.error
    );
    let skill_count = list_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected array result"))?
        .len();
    assert_eq!(skill_count, 2, "should list both created skills");
    Ok(())
}

#[tokio::test]
async fn skill_delete_removes_skill() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let create_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::SkillCreate {
            name: "to-delete".to_string(),
            content: "# Delete Me".to_string(),
        },
    };
    let create_resp = handle_request(&state, create_req).await.expect("response");
    let skill_id_str = create_resp.result.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let skill_id = harness_core::types::SkillId::from_str(&skill_id_str);

    let delete_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::SkillDelete { skill_id },
    };
    let delete_resp = handle_request(&state, delete_req).await.expect("response");
    assert!(
        delete_resp.error.is_none(),
        "skill_delete should succeed: {:?}",
        delete_resp.error
    );
    let result = delete_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["deleted"], serde_json::json!(true));
    Ok(())
}

#[tokio::test]
async fn context_rpc_preview_with_supplied_items_returns_manifest() -> anyhow::Result<()> {
    use harness_protocol::context::{
        ContextPreviewDegraded, ContextPreviewItem, ContextPreviewItemClass, ContextPreviewItemId,
        ContextPreviewPriority, ContextPreviewRequest, ContextPreviewTaskProfile,
    };

    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let request = ContextPreviewRequest {
        thread_id: harness_core::types::ThreadId::from_str("thread-preview"),
        run_id: None,
        project: harness_core::types::ProjectId::from_str("project-preview"),
        task_profile: ContextPreviewTaskProfile {
            prompt: Some("preview supplied context".to_string()),
            ..Default::default()
        },
        budget_hint: 100,
    };
    let supplied = vec![ContextPreviewItem {
        id: ContextPreviewItemId::new("rule:supplied"),
        class: ContextPreviewItemClass::Rule,
        content: "Follow the supplied rule.".to_string(),
        est_tokens: 0,
        priority: ContextPreviewPriority::P1,
        relevance: 1.0,
        degrade: vec![ContextPreviewDegraded::Pointer(
            "See supplied rule.".to_string(),
        )],
        dedupe_key: None,
        instruction_bearing: true,
    }];

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ContextPreview {
            request,
            supplied_items: supplied,
        },
    };
    let resp = handle_request(&state, req).await.expect("response");
    assert!(
        resp.error.is_none(),
        "context preview should succeed: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert!(result["rendered"]
        .as_str()
        .unwrap_or("")
        .contains("supplied rule"));
    assert_eq!(result["manifest"]["mode"], serde_json::json!("preview"));
    let manifest_items = result["manifest"]["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest items should be an array"))?;
    assert!(manifest_items
        .iter()
        .any(|item| item["id"] == serde_json::json!("rule:supplied")));
    Ok(())
}

#[tokio::test]
async fn context_preview_uses_current_run_id_when_request_omits_run_id() -> anyhow::Result<()> {
    use harness_protocol::context::{ContextPreviewRequest, ContextPreviewTaskProfile};

    let _env_lock = CONTEXT_RUN_ID_ENV_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let run_id = "ar-01j1qb3c9r7v5m2k8x4tznq6wf";
    let _env_guard = ContextRunIdEnvGuard::set(run_id);
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::ContextPreview {
            request: ContextPreviewRequest {
                thread_id: harness_core::types::ThreadId::from_str("thread-run-fallback"),
                run_id: None,
                project: harness_core::types::ProjectId::from_str("project-run-fallback"),
                task_profile: ContextPreviewTaskProfile::default(),
                budget_hint: 100,
            },
            supplied_items: Vec::new(),
        },
    };

    let resp = handle_request(&state, req).await.expect("response");
    assert!(
        resp.error.is_none(),
        "context preview should succeed: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["manifest"]["run_id"], serde_json::json!(run_id));
    Ok(())
}

#[tokio::test]
async fn context_preview_preserves_composer_error_mapping() -> anyhow::Result<()> {
    use harness_protocol::context::{
        ContextPreviewItem, ContextPreviewItemClass, ContextPreviewItemId, ContextPreviewPriority,
        ContextPreviewRequest, ContextPreviewTaskProfile,
    };

    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(3)),
        method: Method::ContextPreview {
            request: ContextPreviewRequest {
                thread_id: harness_core::types::ThreadId::from_str("thread-overflow"),
                run_id: None,
                project: harness_core::types::ProjectId::from_str("project-overflow"),
                task_profile: ContextPreviewTaskProfile::default(),
                budget_hint: 1,
            },
            supplied_items: vec![ContextPreviewItem {
                id: ContextPreviewItemId::new("rule:mandatory-overflow"),
                class: ContextPreviewItemClass::Rule,
                content: "mandatory content that cannot fit in the effective budget".repeat(4),
                est_tokens: 0,
                priority: ContextPreviewPriority::P0,
                relevance: 1.0,
                degrade: Vec::new(),
                dedupe_key: None,
                instruction_bearing: true,
            }],
        },
    };

    let resp = handle_request(&state, req).await.expect("response");
    let error = resp
        .error
        .ok_or_else(|| anyhow::anyhow!("expected composer error"))?;
    assert_eq!(error.code, harness_protocol::methods::INTERNAL_ERROR);
    assert!(
        error
            .message
            .contains("mandatory context exceeds effective budget"),
        "unexpected error message: {}",
        error.message
    );
    assert!(resp.result.is_none());
    Ok(())
}

#[tokio::test]
async fn stats_query_returns_expected_shape() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::StatsQuery {
            since: None,
            until: None,
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_none(),
        "stats_query should succeed: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert!(
        result["hook_stats"].is_array(),
        "result should contain hook_stats array"
    );
    assert!(
        result["rule_stats"].is_array(),
        "result should contain rule_stats array"
    );
    Ok(())
}

#[tokio::test]
async fn learn_rules_returns_zero_when_no_adopted_drafts() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-learn-rules-test-")?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::LearnRules {
            project_root: proj_dir.path().to_path_buf(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_none(),
        "learn_rules should succeed with no drafts: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["rules_learned"], serde_json::json!(0));
    assert!(
        result["rules"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "rules array should be empty"
    );
    Ok(())
}

#[tokio::test]
async fn learn_skills_returns_zero_when_no_adopted_drafts() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-learn-skills-test-")?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::LearnSkills {
            project_root: proj_dir.path().to_path_buf(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_none(),
        "learn_skills should succeed with no drafts: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["skills_learned"], serde_json::json!(0));
    assert!(
        result["skills"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "skills array should be empty"
    );
    Ok(())
}

// --- ExecPlan persistence tests ---
