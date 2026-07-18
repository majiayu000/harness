use super::*;
use crate::{
    http::AppState,
    server::HarnessServer,
    test_helpers::{make_test_state, make_test_state_with_registry},
    thread_manager::ThreadManager,
};
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use harness_protocol::{methods::Method, methods::RpcRequest, methods::VALIDATION_ERROR};
use std::sync::Arc;
use tokio::sync::RwLock;

#[path = "tests/exec_plan.rs"]
mod exec_plan;

#[path = "tests/observability.rs"]
mod observability;

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
