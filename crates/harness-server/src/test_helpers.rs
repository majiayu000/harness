use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64},
    Arc,
};

/// Serialises every test that reads or mutates the process-global `HOME` env
/// var.  `tokio::test` runs tests concurrently in the same process; without
/// this lock, a test that temporarily changes `HOME` races with any other test
/// that calls `validate_project_root` (which reads `HOME`), leading to
/// spurious "project root must be within HOME" failures.
pub static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// RAII guard that restores `HOME` on drop, **including on panic**.
pub struct HomeGuard {
    original: Option<String>,
}

impl HomeGuard {
    /// Overwrite `HOME` with `path` and return a guard that restores it.
    ///
    /// # Safety
    /// The caller must hold `HOME_LOCK` for the lifetime of this guard.
    pub unsafe fn set(path: &std::path::Path) -> Self {
        let original = std::env::var("HOME").ok();
        std::env::set_var("HOME", path);
        HomeGuard { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match self.original.take() {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::AgentRegistry;
use harness_core::HarnessConfig;
use tokio::sync::RwLock;

/// Create a temp directory under a writable base path without mutating
/// global state (`HOME` env var).  Tries `$HOME` first; falls back to
/// `$CWD/.harness-test-home` if `$HOME` is not writable.
pub fn tempdir_in_home(prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("resolve cwd"));
    if let Ok(dir) = tempfile::Builder::new().prefix(prefix).tempdir_in(&home) {
        return Ok(dir);
    }
    let fallback = std::env::current_dir()?.join(".harness-test-home");
    std::fs::create_dir_all(&fallback)?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&fallback)
        .map_err(Into::into)
}

pub async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    make_test_state_with_registry(dir, AgentRegistry::new("test")).await
}

pub async fn make_test_state_with_registry(
    dir: &std::path::Path,
    agent_registry: AgentRegistry,
) -> anyhow::Result<AppState> {
    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
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
    Ok(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            tasks,
            thread_db: Some(thread_db),
            plan_db: None,
            project_registry: None,
        },
        engines: crate::http::EngineServices {
            skills: Default::default(),
            rules: Default::default(),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(crate::http::SignalRateLimiter::new(100)),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
        notifications: crate::http::NotificationServices {
            notification_tx,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        feishu_intake: None,
        github_intake: None,
        completion_callback: None,
    })
}

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
    Ok(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            tasks,
            thread_db: Some(thread_db),
            plan_db: None,
            project_registry: None,
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(crate::http::SignalRateLimiter::new(100)),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
        notifications: crate::http::NotificationServices {
            notification_tx,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        feishu_intake: None,
        github_intake: None,
        completion_callback: None,
    })
}

pub async fn make_test_state_with_plan_db(dir: &std::path::Path) -> anyhow::Result<AppState> {
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
    Ok(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            tasks,
            thread_db: Some(thread_db),
            plan_db: Some(plan_db),
            project_registry: None,
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(crate::http::SignalRateLimiter::new(100)),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
        notifications: crate::http::NotificationServices {
            notification_tx,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        feishu_intake: None,
        github_intake: None,
        completion_callback: None,
    })
}
