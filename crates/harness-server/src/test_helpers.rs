use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64},
    Arc, OnceLock,
};
use std::time::Duration;

use harness_core::db::pg_open_pool;
use tokio::sync::OnceCell;

/// Serialises every test that reads or mutates the process-global `HOME` env
/// var.  `tokio::test` runs tests concurrently in the same process; without
/// this lock, a test that temporarily changes `HOME` races with any other test
/// that calls `validate_project_root` (which reads `HOME`), leading to
/// spurious "project root must be within HOME" failures.
pub static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static DB_AVAILABLE: OnceCell<bool> = OnceCell::const_new();
static DB_STATE_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

fn db_state_lock() -> Arc<tokio::sync::Mutex<()>> {
    DB_STATE_LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

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
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use sha2::{Digest, Sha256};

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

pub async fn db_tests_enabled() -> bool {
    if std::env::var("DATABASE_URL").is_err() {
        return false;
    }

    *DB_AVAILABLE
        .get_or_init(|| async {
            let Ok(database_url) = std::env::var("DATABASE_URL") else {
                return false;
            };
            match tokio::time::timeout(Duration::from_secs(2), pg_open_pool(&database_url)).await {
                Ok(Ok(pool)) => {
                    pool.close().await;
                    true
                }
                _ => false,
            }
        })
        .await
}

pub async fn acquire_db_state_guard() -> tokio::sync::OwnedMutexGuard<()> {
    db_state_lock().lock_owned().await
}

pub fn is_pool_timeout(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("pool timed out while waiting for an open connection")
}

pub async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    make_state_inner(
        dir,
        dir,
        AgentRegistry::new("test"),
        HarnessConfig::default(),
    )
    .await
}

pub async fn drop_tasks_table(dir: &std::path::Path) -> anyhow::Result<()> {
    let db_path = harness_core::config::dirs::default_db_path(dir, "tasks");
    let database_url = harness_core::db::resolve_database_url(None)?;
    let path_utf8 = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", db_path))?;
    let digest = Sha256::digest(path_utf8.as_bytes());
    let mut schema_bytes = [0u8; 8];
    schema_bytes.copy_from_slice(&digest[..8]);
    let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));
    let pool = harness_core::db::pg_open_pool_schematized(&database_url, &schema).await?;
    sqlx::query("DROP TABLE tasks CASCADE")
        .execute(&pool)
        .await?;
    pool.close().await;
    Ok(())
}

pub async fn make_test_state_with_registry(
    dir: &std::path::Path,
    agent_registry: AgentRegistry,
) -> anyhow::Result<AppState> {
    make_state_inner(dir, dir, agent_registry, HarnessConfig::default()).await
}

/// Build a test `AppState` wrapped in `Arc`, using separate data and project-root directories.
///
/// Use this when the test needs to assert that the server uses the configured
/// `project_root` and not the process working directory.
pub async fn make_test_state_with_project_root(
    dir: &std::path::Path,
    project_root: &std::path::Path,
) -> anyhow::Result<Arc<AppState>> {
    Ok(Arc::new(
        make_state_inner(
            dir,
            project_root,
            AgentRegistry::new("test"),
            HarnessConfig::default(),
        )
        .await?,
    ))
}

async fn make_state_inner(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    agent_registry: AgentRegistry,
    config: HarnessConfig,
) -> anyhow::Result<AppState> {
    #[cfg(test)]
    let db_state_guard = Some(acquire_db_state_guard().await);
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        agent_registry,
    ));
    let password_reset_rate_limit = server.config.server.password_reset_rate_limit_per_hour;
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
        project_root.to_path_buf(),
    ));
    let thread_db = crate::thread_db::ThreadDb::open(&harness_core::config::dirs::default_db_path(
        dir, "threads",
    ))
    .await?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(64);
    let task_queue = Arc::new(crate::task_queue::TaskQueue::new(&Default::default()));

    // Service layer — use concrete defaults backed by the same infrastructure.
    let project_svc = crate::services::project::DefaultProjectService::new(
        // Tests that don't need a registry still get a lightweight one.
        crate::project_registry::ProjectRegistry::open(
            &harness_core::config::dirs::default_db_path(dir, "projects"),
        )
        .await?,
        project_root.to_path_buf(),
    );
    let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        task_queue.clone(),
        task_queue.clone(),
        None,
        None,
        None,
        vec![],
    );

    Ok(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: project_root.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| project_root.to_path_buf()),
            tasks,
            thread_db: Some(thread_db),
            plan_db: None,
            plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
            issue_workflow_store: None,
            project_workflow_store: None,
            project_registry: None,
            runtime_state_store: None,
            q_values: None,
            maintenance_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        engines: crate::http::EngineServices {
            skills: Default::default(),
            rules: Default::default(),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(crate::http::rate_limit::SignalRateLimiter::new(100)),
            password_reset_rate_limiter: Arc::new(
                crate::http::rate_limit::PasswordResetRateLimiter::new(password_reset_rate_limit),
            ),
            review_store: None,
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue,
            review_task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
        #[cfg(test)]
        _db_state_guard: db_state_guard,
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
        notifications: crate::http::NotificationServices {
            notification_tx,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(true)),
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        degraded_subsystems: vec![],
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
