use super::*;

#[tokio::test]
async fn exec_plan_init_persists_to_db() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let state = super::make_test_state_with_plan_db(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ExecPlanInit {
            spec: "# Persist to DB\n\nTest plan.".to_string(),
            project_root: proj_dir.path().to_path_buf(),
        },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response");
    assert!(
        resp.error.is_none(),
        "init should succeed: {:?}",
        resp.error
    );

    let plan_id_str = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?["plan_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("plan_id not a string"))?
        .to_string();

    let plan_id = harness_core::types::ExecPlanId(plan_id_str);
    let db = state.core.plan_db.as_ref().unwrap();
    let stored = db
        .get(&plan_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("plan should be in DB"))?;
    assert_eq!(stored.purpose, "Persist to DB");
    Ok(())
}

#[tokio::test]
async fn exec_plan_status_reads_plan_from_memory() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let state = super::make_test_state_with_plan_db(dir.path()).await?;

    let init_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ExecPlanInit {
            spec: "# Status Test".to_string(),
            project_root: proj_dir.path().to_path_buf(),
        },
    };
    let init_resp = handle_request(&state, init_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response"))?;
    assert!(
        init_resp.error.is_none(),
        "init should succeed: {:?}",
        init_resp.error
    );
    let plan_id_str = init_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?["plan_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("plan_id not a string"))?
        .to_string();

    let status_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::ExecPlanStatus {
            plan_id: harness_core::types::ExecPlanId(plan_id_str),
        },
    };
    let status_resp = handle_request(&state, status_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response"))?;
    assert!(
        status_resp.error.is_none(),
        "status should succeed: {:?}",
        status_resp.error
    );
    let result = status_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["purpose"], "Status Test");
    assert_eq!(result["status"], "draft");
    Ok(())
}

#[tokio::test]
async fn exec_plan_update_persists_status_change() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let state = super::make_test_state_with_plan_db(dir.path()).await?;

    let init_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ExecPlanInit {
            spec: "# Update Test".to_string(),
            project_root: proj_dir.path().to_path_buf(),
        },
    };
    let init_resp = handle_request(&state, init_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response"))?;
    assert!(
        init_resp.error.is_none(),
        "init should succeed: {:?}",
        init_resp.error
    );
    let plan_id_str = init_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?["plan_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("plan_id not a string"))?
        .to_string();
    let plan_id = harness_core::types::ExecPlanId(plan_id_str);

    let update_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::ExecPlanUpdate {
            plan_id: plan_id.clone(),
            updates: serde_json::json!({ "action": "activate" }),
        },
    };
    let update_resp = handle_request(&state, update_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response"))?;
    assert!(
        update_resp.error.is_none(),
        "update should succeed: {:?}",
        update_resp.error
    );

    let db = state
        .core
        .plan_db
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("plan_db should be set"))?;
    let stored = db
        .get(&plan_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("plan should be in DB"))?;
    assert_eq!(stored.status, harness_core::types::ExecPlanStatus::Active);
    Ok(())
}

#[tokio::test]
async fn exec_plan_survives_simulated_restart() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let data_dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let plan_db_path = harness_core::config::dirs::default_db_path(data_dir.path(), "plans");
    let plan_id_str: String;

    // Session 1: create and activate a plan.
    {
        let state = super::make_test_state_with_plan_db(data_dir.path()).await?;
        let init_req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ExecPlanInit {
                spec: "# Restart Test".to_string(),
                project_root: proj_dir.path().to_path_buf(),
            },
        };
        let init_resp = handle_request(&state, init_req)
            .await
            .ok_or_else(|| anyhow::anyhow!("expected response"))?;
        assert!(
            init_resp.error.is_none(),
            "init should succeed: {:?}",
            init_resp.error
        );
        plan_id_str = init_resp
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?["plan_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("plan_id not a string"))?
            .to_string();

        let update_req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(2)),
            method: Method::ExecPlanUpdate {
                plan_id: harness_core::types::ExecPlanId(plan_id_str.clone()),
                updates: serde_json::json!({ "action": "activate" }),
            },
        };
        let update_resp = handle_request(&state, update_req)
            .await
            .ok_or_else(|| anyhow::anyhow!("expected response"))?;
        assert!(
            update_resp.error.is_none(),
            "activate should succeed: {:?}",
            update_resp.error
        );
    } // state is dropped here — simulates server shutdown

    // Session 2: fresh DB open — simulates restart.
    {
        let plan_db = crate::plan_db::PlanDb::open(&plan_db_path).await?;
        let persisted = plan_db.list().await?;
        let mut map = std::collections::HashMap::new();
        for plan in persisted {
            map.insert(plan.id.clone(), plan);
        }

        let pid = harness_core::types::ExecPlanId(plan_id_str.clone());
        let recovered = map
            .get(&pid)
            .ok_or_else(|| anyhow::anyhow!("plan should survive restart"))?;
        assert_eq!(recovered.purpose, "Restart Test");
        assert_eq!(
            recovered.status,
            harness_core::types::ExecPlanStatus::Active
        );
    }
    Ok(())
}

#[tokio::test]
async fn exec_plan_status_fallback_to_db_when_not_in_memory() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let data_dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let plan_db_path = harness_core::config::dirs::default_db_path(data_dir.path(), "plans");

    // Insert a plan directly into the DB without going through the in-memory HashMap.
    let plan = harness_exec::plan::ExecPlan::from_spec("# Direct DB Insert", proj_dir.path())?;
    let plan_id = plan.id.clone();
    {
        let db = crate::plan_db::PlanDb::open(&plan_db_path).await?;
        db.upsert(&plan).await?;
    }

    // Create state with the DB but empty HashMap (simulates post-restart before warmup).
    let state = super::make_test_state_with_plan_db(data_dir.path()).await?;

    // ExecPlanStatus must fall back to DB when plan is absent from HashMap.
    let status_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ExecPlanStatus { plan_id },
    };
    let resp = handle_request(&state, status_req)
        .await
        .ok_or_else(|| anyhow::anyhow!("expected response"))?;
    assert!(
        resp.error.is_none(),
        "status should fall back to DB: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(result["purpose"], "Direct DB Insert");
    Ok(())
}
