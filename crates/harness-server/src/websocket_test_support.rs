//! Shared test support for WebSocket tests: builds a real `AppState` backed
//! by tempdir stores and provides sandbox-tolerant TCP/WS client helpers.
//! Extracted from `websocket.rs` so both the connection-loop tests and the
//! dispatch-worker tests can use them without pushing `websocket.rs` over the
//! file-size limit.

#![cfg(test)]

use crate::http::AppState;
use crate::server::HarnessServer;
use crate::thread_manager::ThreadManager;
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::RwLock;

pub(crate) type TestWebSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    make_test_state_with_config(dir, HarnessConfig::default()).await
}

/// Explicitly test-gated so storage-opener guards classify these tempdir
/// store opens as test usage (lib.rs already compiles this module only for
/// tests; the attribute keeps this file self-describing).
#[cfg(test)]
pub(crate) async fn make_test_state_with_config(
    dir: &std::path::Path,
    mut config: HarnessConfig,
) -> anyhow::Result<AppState> {
    config.server.allow_unauthenticated = true;
    let db_setup_guard = crate::test_helpers::acquire_db_state_guard().await;
    let notification_broadcast_capacity = config.server.notification_broadcast_capacity.max(1);
    let notification_lag_log_every = config.server.notification_lag_log_every;
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let tasks = crate::task_runner::TaskStore::open(&harness_core::config::dirs::default_db_path(
        dir, "tasks",
    ))
    .await?;
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir).await?);
    let signal_detector = harness_gc::signal_detector::SignalDetector::new(
        server.config.gc.signal_thresholds.clone(),
        harness_core::types::ProjectId::new(),
    );
    let draft_store = harness_gc::draft_store::DraftStore::new(dir)?;
    let gc_agent = Arc::new(harness_gc::gc_agent::GcAgent::new(
        server.config.gc.clone(),
        signal_detector,
        draft_store,
        dir.to_path_buf(),
    ));
    let (notification_tx, _) = broadcast::channel(notification_broadcast_capacity);
    let (ws_shutdown_tx, _) = broadcast::channel(1);

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
    drop(db_setup_guard);

    Ok(AppState {
        background_loops: Arc::new(crate::http::background::BackgroundLoopHealth::new()),
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
        _db_state_guard: None,
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
            notification_lag_log_every,
            notify_tx: None,
            initializing: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            ws_shutdown_tx,
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

pub(crate) async fn bind_ws_test_listener() -> anyhow::Result<Option<tokio::net::TcpListener>> {
    match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => Ok(Some(listener)),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!("skipping websocket test due to sandbox network restriction: {err}");
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

pub(crate) async fn connect_ws_test_client(url: &str) -> anyhow::Result<Option<TestWebSocket>> {
    match tokio_tungstenite::connect_async(url).await {
        Ok((ws, _)) => Ok(Some(ws)),
        Err(tokio_tungstenite::tungstenite::Error::Io(err))
            if err.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            tracing::warn!("skipping websocket test due to sandbox network restriction: {err}");
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}
