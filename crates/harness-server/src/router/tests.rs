use super::*;
use crate::{
    http::AppState,
    server::HarnessServer,
    test_helpers::{make_test_state, make_test_state_with_registry},
    thread_manager::ThreadManager,
};
use harness_agents::registry::AgentRegistry;
use harness_core::{
    agent::{AgentAdapter, AgentEvent, TurnRequest},
    config::HarnessConfig,
};
use harness_protocol::{methods::Method, methods::RpcRequest, methods::VALIDATION_ERROR};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::RwLock;

async fn make_test_state_with_config_and_registry(
    dir: &std::path::Path,
    config: HarnessConfig,
    agent_registry: AgentRegistry,
) -> anyhow::Result<AppState> {
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        agent_registry,
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
    let thread_db = crate::thread_db::ThreadDb::open(&harness_core::config::dirs::default_db_path(
        dir, "threads",
    ))
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
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        None,
        None,
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
            thread_db: Some(thread_db),
            plan_db: None,
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
            review_store: None,
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
        },
        project_svc,
        task_svc,
        execution_svc,
    })
}

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
async fn gc_adopt_response_includes_task_id() -> anyhow::Result<()> {
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
    };

    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let draft_id = DraftId::new();
    let signal = Signal::new(
        SignalType::RepeatedWarn,
        ProjectId::new(),
        serde_json::json!("test signal"),
        RemediationType::Guard,
    );
    let draft = Draft {
        id: draft_id.clone(),
        status: DraftStatus::Pending,
        signal,
        artifacts: vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: std::path::PathBuf::from("test-guard.sh"),
            content: "#!/bin/bash\necho ok".to_string(),
        }],
        rationale: "test".to_string(),
        validation: "test".to_string(),
        generated_at: chrono::Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::GcAdopt { draft_id },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    let err = resp
        .error
        .ok_or_else(|| anyhow::anyhow!("expected error when default agent is missing"))?;
    assert_eq!(
        err.code,
        harness_protocol::methods::INTERNAL_ERROR,
        "gc_adopt should fail fast when auto_pr is enabled but no default agent exists"
    );
    assert!(
        err.message
            .contains("auto_pr requires a registered default agent"),
        "unexpected error message: {}",
        err.message
    );
    Ok(())
}

struct MockAgent;

#[async_trait::async_trait]
impl harness_core::agent::CodeAgent for MockAgent {
    fn name(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> Vec<harness_core::types::Capability> {
        vec![]
    }
    async fn execute(
        &self,
        _req: harness_core::agent::AgentRequest,
    ) -> harness_core::error::Result<harness_core::agent::AgentResponse> {
        Ok(harness_core::agent::AgentResponse {
            output: "LGTM".to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: harness_core::types::TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }
    async fn execute_stream(
        &self,
        _req: harness_core::agent::AgentRequest,
        _tx: tokio::sync::mpsc::Sender<harness_core::agent::StreamItem>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
}

struct NonLgtmAgent {
    calls: std::sync::atomic::AtomicUsize,
}

impl NonLgtmAgent {
    fn new() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl harness_core::agent::CodeAgent for NonLgtmAgent {
    fn name(&self) -> &str {
        "non-lgtm"
    }

    fn capabilities(&self) -> Vec<harness_core::types::Capability> {
        vec![]
    }

    async fn execute(
        &self,
        _req: harness_core::agent::AgentRequest,
    ) -> harness_core::error::Result<harness_core::agent::AgentResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = if call == 0 {
            "Implemented\nPR_URL=https://github.com/example/repo/pull/123".to_string()
        } else {
            "Needs follow-up\nFIXED".to_string()
        };
        Ok(harness_core::agent::AgentResponse {
            output,
            stderr: String::new(),
            items: vec![],
            token_usage: harness_core::types::TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: harness_core::agent::AgentRequest,
        tx: tokio::sync::mpsc::Sender<harness_core::agent::StreamItem>,
    ) -> harness_core::error::Result<()> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = if call == 0 {
            "Implemented\nPR_URL=https://github.com/example/repo/pull/123\n"
        } else {
            "Needs follow-up\nFIXED\n"
        };
        if tx
            .send(harness_core::agent::StreamItem::MessageDelta {
                text: output.to_string(),
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        // Receiver may have dropped; ignore send error for Done sentinel.
        tx.send(harness_core::agent::StreamItem::Done).await.ok();
        Ok(())
    }
}

async fn wait_for_terminal_task(
    state: &AppState,
    task_id: &str,
) -> anyhow::Result<crate::task_runner::TaskState> {
    use tokio::time::{sleep, Duration, Instant};

    let tid = harness_core::types::TaskId(task_id.to_string());
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_status = None;
    loop {
        if let Some(task) = state.core.tasks.get(&tid) {
            last_status = Some(format!("{:?}", task.status));
            if matches!(
                task.status,
                crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
            ) {
                return Ok(task);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("task did not reach terminal state in time; last_status={last_status:?}");
}

async fn run_gc_adopt_and_wait_for_failure_turn(max_rounds: u32) -> anyhow::Result<u32> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-gc-test-")?;
    let mut config = HarnessConfig::default();
    config.gc.adopt_wait_secs = 0;
    config.gc.adopt_max_rounds = max_rounds;
    config.gc.adopt_turn_timeout_secs = 30;
    // This test verifies the legacy hosted-bot review loop exhaustion path.
    // The production default keeps hosted bots advisory and disabled.
    config.agents.review.review_bot_auto_trigger = true;
    // Disable Jaccard loop detection so this test can verify max_rounds exhaustion.
    // NonLgtmAgent intentionally returns identical output every round.
    config.concurrency.loop_jaccard_threshold = 1.1;

    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", Arc::new(NonLgtmAgent::new()));
    let state = make_test_state_with_config_and_registry(dir.path(), config, registry).await?;

    let draft_id = harness_core::types::DraftId::new();
    let signal = harness_core::types::Signal::new(
        harness_core::types::SignalType::RepeatedWarn,
        harness_core::types::ProjectId::new(),
        serde_json::json!("test signal"),
        harness_core::types::RemediationType::Guard,
    );
    let draft = harness_core::types::Draft {
        id: draft_id.clone(),
        status: harness_core::types::DraftStatus::Pending,
        signal,
        artifacts: vec![harness_core::types::Artifact {
            artifact_type: harness_core::types::ArtifactType::Guard,
            target_path: std::path::PathBuf::from("test-guard.sh"),
            content: "#!/bin/bash\necho ok".to_string(),
        }],
        rationale: "test".to_string(),
        validation: "test".to_string(),
        generated_at: chrono::Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::GcAdopt { draft_id },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");
    assert!(
        resp.error.is_none(),
        "expected success, got error: {:?}",
        resp.error
    );

    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let task_id = result["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("task_id should be a string"))?;

    let task = wait_for_terminal_task(&state, task_id).await?;
    assert!(
        matches!(task.status, crate::task_runner::TaskStatus::Failed),
        "expected task to fail after exhausting rounds, got {:?}",
        task.status
    );
    Ok(task.turn)
}

#[tokio::test]
async fn gc_adopt_spawns_task_when_agent_registered() -> anyhow::Result<()> {
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
    };

    let dir = tempfile::tempdir()?;
    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", Arc::new(MockAgent));
    let state = make_test_state_with_registry(dir.path(), registry).await?;

    let draft_id = DraftId::new();
    let signal = Signal::new(
        SignalType::RepeatedWarn,
        ProjectId::new(),
        serde_json::json!("test signal"),
        RemediationType::Guard,
    );
    let draft = Draft {
        id: draft_id.clone(),
        status: DraftStatus::Pending,
        signal,
        artifacts: vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: std::path::PathBuf::from("test-guard.sh"),
            content: "#!/bin/bash\necho ok".to_string(),
        }],
        rationale: "test".to_string(),
        validation: "test".to_string(),
        generated_at: chrono::Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::GcAdopt { draft_id },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    assert!(
        resp.error.is_none(),
        "expected success, got error: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    assert_eq!(
        result["adopted"],
        serde_json::json!(true),
        "adopted must be true"
    );
    let task_id = result["task_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("task_id should be a string"))?;
    assert!(!task_id.is_empty(), "task_id should be non-empty");
    let tid = harness_core::types::TaskId(task_id.to_string());
    let task = state.core.tasks.get(&tid);
    assert!(task.is_some(), "task should exist in the task store");
    Ok(())
}

#[tokio::test]
async fn gc_adopt_schedule_changes_with_gc_config() -> anyhow::Result<()> {
    let short_max_rounds = 1;
    let long_max_rounds = 3;
    let short_schedule_turn = run_gc_adopt_and_wait_for_failure_turn(short_max_rounds).await?;
    let long_schedule_turn = run_gc_adopt_and_wait_for_failure_turn(long_max_rounds).await?;

    assert_eq!(
        short_schedule_turn,
        short_max_rounds + 1,
        "max_rounds=1 should end at turn 2"
    );
    assert_eq!(
        long_schedule_turn,
        long_max_rounds + 1,
        "max_rounds=3 should end at turn 4"
    );
    assert!(
        long_schedule_turn > short_schedule_turn,
        "larger max_rounds should produce a longer schedule"
    );
    Ok(())
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
async fn usage_probe_counts_thread_and_turn_rpc_dispatch() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let before = harness_core::usage_probe::snapshot();
    let thread_before = before
        .iter()
        .find(|entry| entry.surface == "thread_rpc")
        .map(|entry| entry.count)
        .unwrap_or(0);
    let turn_before = before
        .iter()
        .find(|entry| entry.surface == "turn_rpc")
        .map(|entry| entry.count)
        .unwrap_or(0);
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let thread_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadList,
    };
    let turn_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::TurnStatus {
            turn_id: harness_core::types::TurnId::from_str("missing-turn"),
        },
    };

    let _ = handle_request(&state, thread_req).await;
    let _ = handle_request(&state, turn_req).await;

    let counts = harness_core::usage_probe::snapshot();
    let thread_rpc = counts
        .iter()
        .find(|entry| entry.surface == "thread_rpc")
        .ok_or_else(|| anyhow::anyhow!("missing thread_rpc count"))?;
    let turn_rpc = counts
        .iter()
        .find(|entry| entry.surface == "turn_rpc")
        .ok_or_else(|| anyhow::anyhow!("missing turn_rpc count"))?;

    assert!(thread_rpc.count > thread_before);
    assert!(turn_rpc.count > turn_before);
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
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let request = harness_context::ComposeRequest {
        thread_id: harness_core::types::ThreadId::from_str("thread-preview"),
        run_id: None,
        project: harness_core::types::ProjectId::from_str("project-preview"),
        task_profile: harness_context::TaskProfile {
            prompt: Some("preview supplied context".to_string()),
            ..Default::default()
        },
        budget_hint: 100,
    };
    let supplied = vec![harness_context::ContextItem {
        id: harness_context::ItemId::new("rule:supplied"),
        class: harness_context::ItemClass::Rule,
        content: "Follow the supplied rule.".to_string(),
        est_tokens: 0,
        priority: harness_context::Priority::P1,
        relevance: 1.0,
        degrade: vec![harness_context::Degraded::Pointer(
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
async fn context_shadow_thread_start_logs_manifest_without_response_shape_change(
) -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-context-shadow-")?;
    std::fs::write(
        proj_dir.path().join("AGENTS.md"),
        "Project instruction body",
    )?;
    std::fs::write(
        proj_dir.path().join("WORKFLOW.md"),
        "---\nworkflow:\n  id: context-shadow-test\n---\nWorkflow prompt body\n",
    )?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadStart {
            cwd: proj_dir.path().to_path_buf(),
        },
    };
    let resp = handle_request(&state, req).await.expect("response");
    assert!(
        resp.error.is_none(),
        "thread_start should preserve success: {:?}",
        resp.error
    );
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let thread_id = result["thread_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing thread_id"))?
        .to_string();
    assert_eq!(
        result.as_object().map(|object| object.len()),
        Some(1),
        "shadow mode must not add response fields"
    );

    let get_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(2)),
        method: Method::ContextManifestGet {
            thread_id: harness_core::types::ThreadId::from_str(&thread_id),
        },
    };
    let get_resp = handle_request(&state, get_req).await.expect("response");
    assert!(
        get_resp.error.is_none(),
        "manifest get should succeed: {:?}",
        get_resp.error
    );
    let manifest = get_resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing manifest result"))?;
    assert_eq!(manifest["manifest"]["mode"], serde_json::json!("shadow"));
    assert_eq!(
        manifest["manifest"]["thread_id"],
        serde_json::json!(thread_id)
    );
    let items = manifest["manifest"]["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("manifest items should be an array"))?;
    assert!(
        items
            .iter()
            .any(|item| item["id"] == serde_json::json!("brief:task")),
        "thread_start should record a task brief item"
    );
    assert!(
        items
            .iter()
            .any(|item| item["id"] == serde_json::json!("contract:task")),
        "thread_start should record a project/workflow contract item"
    );
    Ok(())
}

#[tokio::test]
async fn context_enforce_thread_start_fails_closed_until_injection_is_wired() -> anyhow::Result<()>
{
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut config = HarnessConfig::default();
    config.context.mode = harness_core::config::misc::ContextMode::Enforce;
    let state =
        make_test_state_with_config_and_registry(dir.path(), config, AgentRegistry::new("claude"))
            .await?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-context-enforce-")?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadStart {
            cwd: proj_dir.path().to_path_buf(),
        },
    };
    let resp = handle_request(&state, req).await.expect("response");
    let error = resp
        .error
        .ok_or_else(|| anyhow::anyhow!("enforce mode should fail closed"))?;

    assert!(
        error.message.contains("context enforce mode"),
        "unexpected error: {}",
        error.message
    );
    assert!(resp.result.is_none());
    Ok(())
}

struct SteerTrackingAdapter {
    steer_called: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl AgentAdapter for SteerTrackingAdapter {
    fn name(&self) -> &str {
        "tracking"
    }

    async fn start_turn(
        &self,
        _req: TurnRequest,
        _tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        Ok(())
    }

    async fn steer(&self, _text: String) -> harness_core::error::Result<()> {
        self.steer_called.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn context_enforce_thread_resume_does_not_mutate_thread_status() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HarnessConfig::default();
    config.context.mode = harness_core::config::misc::ContextMode::Enforce;
    let state =
        make_test_state_with_config_and_registry(dir.path(), config, AgentRegistry::new("claude"))
            .await?;
    let proj_dir = tempfile::tempdir()?;
    let thread_id = state
        .core
        .server
        .thread_manager
        .start_thread(proj_dir.path().to_path_buf());
    {
        let mut thread = state
            .core
            .server
            .thread_manager
            .threads_cache()
            .get_mut(thread_id.as_str())
            .ok_or_else(|| anyhow::anyhow!("thread should exist"))?;
        thread.status = harness_core::types::ThreadStatus::Archived;
    }

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadResume {
            thread_id: thread_id.clone(),
        },
    };
    let resp = handle_request(&state, req).await.expect("response");
    let error = resp
        .error
        .ok_or_else(|| anyhow::anyhow!("enforce mode should fail closed"))?;

    assert!(
        error.message.contains("context enforce mode"),
        "unexpected error: {}",
        error.message
    );
    let thread = state
        .core
        .server
        .thread_manager
        .get_thread(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread should remain present"))?;
    assert_eq!(thread.status, harness_core::types::ThreadStatus::Archived);
    Ok(())
}

#[tokio::test]
async fn context_enforce_turn_steer_does_not_call_adapter_or_append_item() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HarnessConfig::default();
    config.context.mode = harness_core::config::misc::ContextMode::Enforce;
    let state =
        make_test_state_with_config_and_registry(dir.path(), config, AgentRegistry::new("claude"))
            .await?;
    let proj_dir = tempfile::tempdir()?;
    let thread_id = state
        .core
        .server
        .thread_manager
        .start_thread(proj_dir.path().to_path_buf());
    let turn_id = state.core.server.thread_manager.start_turn(
        &thread_id,
        "initial task".to_string(),
        harness_core::types::AgentId::new(),
    )?;
    let steer_called = Arc::new(AtomicBool::new(false));
    state.core.server.thread_manager.register_active_adapter(
        &turn_id,
        Arc::new(SteerTrackingAdapter {
            steer_called: steer_called.clone(),
        }),
    );

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::TurnSteer {
            turn_id: turn_id.clone(),
            instruction: "redirect here".to_string(),
        },
    };
    let resp = handle_request(&state, req).await.expect("response");
    let error = resp
        .error
        .ok_or_else(|| anyhow::anyhow!("enforce mode should fail closed"))?;

    assert!(
        error.message.contains("context enforce mode"),
        "unexpected error: {}",
        error.message
    );
    assert!(
        !steer_called.load(Ordering::SeqCst),
        "adapter steer must not run when context enforce fails"
    );
    let turn = state
        .core
        .server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .ok_or_else(|| anyhow::anyhow!("turn should remain present"))?;
    assert_eq!(
        turn.items.len(),
        1,
        "failed enforce steer must not append a user message"
    );
    Ok(())
}

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

// --- ExecPlan persistence tests ---

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
    let thread_db = crate::thread_db::ThreadDb::open(&harness_core::config::dirs::default_db_path(
        dir, "threads",
    ))
    .await?;
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
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        None,
        None,
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
            thread_db: Some(thread_db),
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
            review_store: None,
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
