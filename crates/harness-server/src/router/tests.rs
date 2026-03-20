use super::*;
use crate::{
    http::AppState,
    server::HarnessServer,
    test_helpers::{make_test_state, make_test_state_with_registry},
    thread_manager::ThreadManager,
};
use harness_agents::AgentRegistry;
use harness_core::HarnessConfig;
use harness_protocol::{Method, RpcRequest, INTERNAL_ERROR};
use std::sync::Arc;
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
    let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
    let events = Arc::new(harness_observe::EventStore::new(dir).await?);
    let signal_detector = harness_gc::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::ProjectId::new(),
    );
    let draft_store = harness_gc::DraftStore::new(dir)?;
    let gc_agent = Arc::new(harness_gc::GcAgent::new(
        server.config.gc.clone(),
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    ));
    let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(64);
    let _project_svc_tmp =
        crate::project_registry::ProjectRegistry::open(&dir.join("svc_projects.db")).await?;
    let project_svc =
        crate::services::DefaultProjectService::new(_project_svc_tmp, dir.to_path_buf());
    let task_svc = crate::services::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        None,
        None,
        vec![],
    );
    Ok(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| dir.to_path_buf()),
            tasks,
            thread_db: Some(thread_db),
            plan_db: None,
            plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
            project_registry: None,
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: std::sync::Arc::new(crate::http::SignalRateLimiter::new(100)),
            review_store: None,
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
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
        intake: crate::http::IntakeServices {
            feishu_intake: None,
            github_intake: None,
            completion_callback: None,
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
    use harness_core::{
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
    assert_eq!(
        result["task_id"],
        serde_json::json!(null),
        "task_id should be null when no agent is registered"
    );
    Ok(())
}

struct MockAgent;

#[async_trait::async_trait]
impl harness_core::CodeAgent for MockAgent {
    fn name(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> Vec<harness_core::Capability> {
        vec![]
    }
    async fn execute(
        &self,
        _req: harness_core::AgentRequest,
    ) -> harness_core::Result<harness_core::AgentResponse> {
        Ok(harness_core::AgentResponse {
            output: "LGTM".to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: harness_core::TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }
    async fn execute_stream(
        &self,
        _req: harness_core::AgentRequest,
        _tx: tokio::sync::mpsc::Sender<harness_core::StreamItem>,
    ) -> harness_core::Result<()> {
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
impl harness_core::CodeAgent for NonLgtmAgent {
    fn name(&self) -> &str {
        "non-lgtm"
    }

    fn capabilities(&self) -> Vec<harness_core::Capability> {
        vec![]
    }

    async fn execute(
        &self,
        _req: harness_core::AgentRequest,
    ) -> harness_core::Result<harness_core::AgentResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = if call == 0 {
            "Implemented\nPR_URL=https://github.com/example/repo/pull/123".to_string()
        } else {
            "Needs follow-up\nFIXED".to_string()
        };
        Ok(harness_core::AgentResponse {
            output,
            stderr: String::new(),
            items: vec![],
            token_usage: harness_core::TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: harness_core::AgentRequest,
        tx: tokio::sync::mpsc::Sender<harness_core::StreamItem>,
    ) -> harness_core::Result<()> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = if call == 0 {
            "Implemented\nPR_URL=https://github.com/example/repo/pull/123\n"
        } else {
            "Needs follow-up\nFIXED\n"
        };
        if tx
            .send(harness_core::StreamItem::MessageDelta {
                text: output.to_string(),
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        // Receiver may have dropped; ignore send error for Done sentinel.
        tx.send(harness_core::StreamItem::Done).await.ok();
        Ok(())
    }
}

async fn wait_for_terminal_task(
    state: &AppState,
    task_id: &str,
) -> anyhow::Result<crate::task_runner::TaskState> {
    use tokio::time::{sleep, Duration};

    let tid = crate::task_runner::TaskId(task_id.to_string());
    for _ in 0..120 {
        if let Some(task) = state.core.tasks.get(&tid) {
            if matches!(
                task.status,
                crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
            ) {
                return Ok(task);
            }
        }
        sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("task did not reach terminal state in time");
}

async fn run_gc_adopt_and_wait_for_failure_turn(max_rounds: u32) -> anyhow::Result<u32> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-gc-test-")?;
    let mut config = HarnessConfig::default();
    config.gc.adopt_wait_secs = 0;
    config.gc.adopt_max_rounds = max_rounds;
    config.gc.adopt_turn_timeout_secs = 30;

    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", Arc::new(NonLgtmAgent::new()));
    let state = make_test_state_with_config_and_registry(dir.path(), config, registry).await?;

    let draft_id = harness_core::DraftId::new();
    let signal = harness_core::Signal::new(
        harness_core::SignalType::RepeatedWarn,
        harness_core::ProjectId::new(),
        serde_json::json!("test signal"),
        harness_core::RemediationType::Guard,
    );
    let draft = harness_core::Draft {
        id: draft_id.clone(),
        status: harness_core::DraftStatus::Pending,
        signal,
        artifacts: vec![harness_core::Artifact {
            artifact_type: harness_core::ArtifactType::Guard,
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
    use harness_core::{
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
    let tid = crate::task_runner::TaskId(task_id.to_string());
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
        .query(&harness_core::EventFilters {
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

    let session_id = harness_core::SessionId::new();
    // The metrics_query handler scopes violation counts by the latest
    // rule_scan session, so we must log a rule_scan summary event first.
    let scan_event = harness_core::Event::new(
        session_id.clone(),
        "rule_scan",
        "RuleEngine",
        harness_core::Decision::Block,
    );
    state.observability.events.log(&scan_event).await?;
    for _ in 0..5 {
        let event = harness_core::Event::new(
            session_id.clone(),
            "rule_check",
            "SEC-01",
            harness_core::Decision::Block,
        );
        state.observability.events.log(&event).await?;
    }

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::MetricsQuery {
            filters: harness_core::MetricFilters::default(),
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
        harness_core::Violation {
            rule_id: harness_core::RuleId::from_str("SEC-01"),
            file: std::path::PathBuf::from("src/lib.rs"),
            line: Some(7),
            message: "critical issue".to_string(),
            severity: harness_core::Severity::Critical,
        },
        harness_core::Violation {
            rule_id: harness_core::RuleId::from_str("U-01"),
            file: std::path::PathBuf::from("src/main.rs"),
            line: None,
            message: "style issue".to_string(),
            severity: harness_core::Severity::Low,
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
            filters: harness_core::MetricFilters::default(),
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
        .query(&harness_core::EventFilters::default())
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
async fn event_log_then_query_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let session_id = harness_core::SessionId::new();
    let event = harness_core::Event::new(
        session_id.clone(),
        "pre_tool_use",
        "Edit",
        harness_core::Decision::Pass,
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
            filters: harness_core::EventFilters {
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
    assert_eq!(resp.error.unwrap().code, harness_protocol::NOT_INITIALIZED);
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
    assert_eq!(resp.error.unwrap().code, harness_protocol::INVALID_REQUEST);
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
    let session_id = harness_core::SessionId::new();
    let event = harness_core::Event::new(
        session_id,
        "test_hook",
        "TestTool",
        harness_core::Decision::Pass,
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
    let session_id = harness_core::SessionId::new();
    let event = harness_core::Event::new(
        session_id,
        "probe_hook",
        "ProbeTarget",
        harness_core::Decision::Pass,
    );
    state.observability.events.log(&event).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::EventQuery {
            filters: harness_core::EventFilters {
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
    let skill_id = harness_core::SkillId::from_str(&skill_id_str);

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
    let skill_id = harness_core::SkillId::from_str(&skill_id_str);

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
async fn thread_resume_errors_for_unknown_thread() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::ThreadResume {
            thread_id: harness_core::ThreadId::new(),
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
            thread_id: harness_core::ThreadId::new(),
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
            thread_id: harness_core::ThreadId::new(),
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
            turn_id: harness_core::TurnId::new(),
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
        harness_protocol::NOT_FOUND,
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
    let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
    let events = Arc::new(harness_observe::EventStore::new(dir).await?);
    let signal_detector = harness_gc::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::ProjectId::new(),
    );
    let draft_store = harness_gc::DraftStore::new(dir)?;
    let gc_agent = Arc::new(harness_gc::GcAgent::new(
        server.config.gc.clone(),
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    ));
    let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
    let plan_db = crate::plan_db::PlanDb::open(&dir.join("exec_plans.db")).await?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(64);
    let _project_svc_tmp =
        crate::project_registry::ProjectRegistry::open(&dir.join("svc_projects.db")).await?;
    let project_svc =
        crate::services::DefaultProjectService::new(_project_svc_tmp, dir.to_path_buf());
    let task_svc = crate::services::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        None,
        None,
        vec![],
    );
    Ok(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| dir.to_path_buf()),
            tasks,
            thread_db: Some(thread_db),
            plan_db: Some(plan_db),
            plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
            project_registry: None,
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: std::sync::Arc::new(crate::http::SignalRateLimiter::new(100)),
            review_store: None,
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
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
        intake: crate::http::IntakeServices {
            feishu_intake: None,
            github_intake: None,
            completion_callback: None,
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

    let plan_id = harness_core::ExecPlanId(plan_id_str);
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
            plan_id: harness_core::ExecPlanId(plan_id_str),
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
    let plan_id = harness_core::ExecPlanId(plan_id_str);

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
    assert_eq!(stored.status, harness_core::ExecPlanStatus::Active);
    Ok(())
}

#[tokio::test]
async fn exec_plan_survives_simulated_restart() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let data_dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let plan_db_path = data_dir.path().join("exec_plans.db");
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
                plan_id: harness_core::ExecPlanId(plan_id_str.clone()),
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

        let pid = harness_core::ExecPlanId(plan_id_str.clone());
        let recovered = map
            .get(&pid)
            .ok_or_else(|| anyhow::anyhow!("plan should survive restart"))?;
        assert_eq!(recovered.purpose, "Restart Test");
        assert_eq!(recovered.status, harness_core::ExecPlanStatus::Active);
    }
    Ok(())
}

#[tokio::test]
async fn exec_plan_status_fallback_to_db_when_not_in_memory() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let data_dir = tempfile::tempdir()?;
    let proj_dir = crate::test_helpers::tempdir_in_home("harness-exec-test-")?;
    let plan_db_path = data_dir.path().join("exec_plans.db");

    // Insert a plan directly into the DB without going through the in-memory HashMap.
    let plan = harness_exec::ExecPlan::from_spec("# Direct DB Insert", proj_dir.path())?;
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
