use super::*;

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
    assert_eq!(error.code, INTERNAL_ERROR);
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
