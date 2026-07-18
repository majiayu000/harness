use super::*;

async fn make_test_state_with_plan_db(dir: &std::path::Path) -> anyhow::Result<AppState> {
    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let tasks = crate::task_runner::TaskStore::open(&harness_core::config::dirs::default_db_path(
        dir, "tasks",
    ))
    .await?;
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir).await?);
    let signal_detector = harness_gc::signal_detector::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::types::ProjectId::new(),
    );
    let draft_store = harness_gc::draft_store::DraftStore::new(dir)?;
    let gc_agent = Arc::new(harness_gc::gc_agent::GcAgent::new(
        server.config.gc.clone(),
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    ));
    let plan_db =
        crate::plan_db::PlanDb::open(&harness_core::config::dirs::default_db_path(dir, "plans"))
            .await?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(64);
    let _project_svc_tmp = crate::project_registry::ProjectRegistry::open(
        &harness_core::config::dirs::default_db_path(dir, "projects"),
    )
    .await?;
    let project_svc =
        crate::services::project::DefaultProjectService::new(_project_svc_tmp, dir.to_path_buf());
    let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        Arc::new(server.config.clone()),
        None,
        None,
        vec![],
    );
    Ok(AppState {
        core: crate::http::CoreServices {
            server: server.clone(),
            project_root: dir.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| dir.to_path_buf()),
            tasks,
            plan_db: Some(plan_db),
            plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
            issue_workflow_store: None,
            project_workflow_store: None,
            workflow_runtime_store: None,
            project_registry: None,
            runtime_state_store: None,
            maintenance_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::store::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            alerts: crate::alerting::AlertHandle::disabled(),
            events,
            signal_rate_limiter: std::sync::Arc::new(
                crate::http::rate_limit::SignalRateLimiter::new(100),
            ),
            password_reset_rate_limiter: std::sync::Arc::new(
                crate::http::rate_limit::PasswordResetRateLimiter::new(5),
            ),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            review_task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
        #[cfg(test)]
        _db_state_guard: Some(crate::test_helpers::acquire_db_state_guard().await),
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        postgres_catalog: crate::postgres_catalog::PostgresCatalogMonitor::unavailable(
            crate::postgres_catalog::PostgresCatalogThresholds::from_server(&server.config.server),
            "postgres_pool_unavailable",
        ),
        isolation_availability: Default::default(),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
        runtime_circuit_breakers: std::sync::Arc::new(
            crate::runtime_circuit_breaker::RuntimeCircuitBreakerRegistry::new(Default::default()),
        ),
        notifications: crate::http::NotificationServices {
            notification_tx,
            notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        startup_statuses: vec![],
        degraded_subsystems: vec![],
        intake: crate::http::IntakeServices {
            feishu_intake: None,
            github_pollers: vec![],
            github_poller_repos: vec![],
            completion_callback: None,
            token_dispatch_counters: crate::http::IntakeServices::new_token_dispatch_counters(),
            intake_bindings: crate::intake::binding::IntakeBindingRegistry::new(),
        },
        project_svc,
        task_svc,
        execution_svc,
    })
}

#[tokio::test]
async fn exec_plan_init_persists_to_db() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let state = make_test_state_with_plan_db(dir.path()).await?;

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
    let state = make_test_state_with_plan_db(dir.path()).await?;

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
    let state = make_test_state_with_plan_db(dir.path()).await?;

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
        let state = make_test_state_with_plan_db(data_dir.path()).await?;
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
    let state = make_test_state_with_plan_db(data_dir.path()).await?;

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
