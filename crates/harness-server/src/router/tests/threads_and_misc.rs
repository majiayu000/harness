use super::*;

#[tokio::test]
async fn thread_resume_errors_for_unknown_thread() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadResume {
            thread_id: harness_core::types::ThreadId::new(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_some(),
        "resume of unknown thread should return error"
    );
    Ok(())
}

#[tokio::test]
async fn thread_fork_errors_for_unknown_thread() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadFork {
            thread_id: harness_core::types::ThreadId::new(),
            from_turn: None,
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_some(),
        "fork of unknown thread should return error"
    );
    Ok(())
}

#[tokio::test]
async fn thread_compact_errors_for_unknown_thread() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadCompact {
            thread_id: harness_core::types::ThreadId::new(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_some(),
        "compact of unknown thread should return error"
    );
    Ok(())
}

#[tokio::test]
async fn turn_steer_returns_not_found_for_unknown_turn() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::TurnSteer {
            turn_id: harness_core::types::TurnId::new(),
            instruction: "redirect here".to_string(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_some(),
        "steer of unknown turn should return error"
    );
    assert_eq!(
        resp.error.unwrap().code,
        harness_protocol::methods::NOT_FOUND,
        "should return NOT_FOUND for unknown turn"
    );
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
