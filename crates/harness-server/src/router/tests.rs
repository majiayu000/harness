// Re-exported for sub-modules via `use super::*;`
pub use crate::{
    http::AppState,
    server::HarnessServer,
    test_helpers::{make_test_state, make_test_state_with_registry},
    thread_manager::ThreadManager,
};
pub use harness_agents::registry::AgentRegistry;
pub use harness_core::config::HarnessConfig;
pub use harness_protocol::{methods::Method, methods::RpcRequest, methods::INTERNAL_ERROR};
pub use std::sync::Arc;
pub use tokio::sync::RwLock;
// Re-export handle_request so sub-modules can reach it via `use super::handle_request`
pub use super::handle_request;

mod events;
mod exec_plan;
mod gc;
mod handshake;
mod metrics_rules;
mod skills;
mod threads_and_misc;

/// Builds a full `AppState` with a custom config and agent registry.
/// Used by GC tests that need to control scheduling parameters.
pub async fn make_test_state_with_config_and_registry(
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
            runtime_state_store: None,
            q_values: None,
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::store::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
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
            workspace_mgr: None,
        },
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
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
            github_pollers: vec![],
            completion_callback: None,
        },
        project_svc,
        task_svc,
        execution_svc,
    })
}

/// Builds a full `AppState` backed by an on-disk `PlanDb`.
/// Used by ExecPlan persistence tests.
pub async fn make_test_state_with_plan_db(dir: &std::path::Path) -> anyhow::Result<AppState> {
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
            runtime_state_store: None,
            q_values: None,
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::store::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
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
            workspace_mgr: None,
        },
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
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
            github_pollers: vec![],
            completion_callback: None,
        },
        project_svc,
        task_svc,
        execution_svc,
    })
}
