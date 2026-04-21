use crate::task_runner;
use dashmap::DashMap;
use harness_protocol::notifications::RpcNotification;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use super::rate_limit;

/// Core services: thread/task management and persistence.
pub struct CoreServices {
    pub server: Arc<crate::server::HarnessServer>,
    pub project_root: std::path::PathBuf,
    /// Home directory captured at startup to avoid TOCTOU when validating
    /// project roots against `$HOME` in concurrent requests.
    pub home_dir: std::path::PathBuf,
    pub tasks: Arc<task_runner::TaskStore>,
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub plan_db: Option<crate::plan_db::PlanDb>,
    /// In-memory plan cache hydrated from `plan_db` on startup.
    /// Write-through: every mutation must also persist via `plan_db`.
    pub plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
    pub project_registry: Option<std::sync::Arc<crate::project_registry::ProjectRegistry>>,
    pub runtime_state_store: Option<Arc<crate::runtime_state_store::RuntimeStateStore>>,
    /// Q-value store for MemRL rule utility tracking. None when unavailable.
    pub q_values: Option<Arc<crate::q_value_store::QValueStore>>,
    /// Set to `true` while the daily maintenance window is active.
    /// Pollers skip dispatch and HTTP POST /tasks returns 503 when this is set.
    /// Initialized to the correct value at startup (handles server starting mid-window).
    pub maintenance_active: Arc<AtomicBool>,
}

/// Engine services: skills, rules, and garbage collection.
pub struct EngineServices {
    pub skills: Arc<tokio::sync::RwLock<harness_skills::store::SkillStore>>,
    pub rules: Arc<tokio::sync::RwLock<harness_rules::engine::RuleEngine>>,
    pub gc_agent: Arc<harness_gc::gc_agent::GcAgent>,
}

/// Observability services: event store and telemetry.
pub struct ObservabilityServices {
    pub events: Arc<harness_observe::event_store::EventStore>,
    pub signal_rate_limiter: Arc<rate_limit::SignalRateLimiter>,
    pub password_reset_rate_limiter: Arc<rate_limit::PasswordResetRateLimiter>,
    pub review_store: Option<Arc<crate::review_store::ReviewStore>>,
}

/// Concurrency services: task queue and workspace isolation.
pub struct ConcurrencyServices {
    pub task_queue: Arc<crate::task_queue::TaskQueue>,
    pub review_task_queue: Arc<crate::task_queue::TaskQueue>,
    pub workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
}

/// Notification services: broadcast channels and lag tracking.
pub struct NotificationServices {
    /// Broadcast channel for server-push notifications (WebSocket and stdio transports).
    pub notification_tx: broadcast::Sender<RpcNotification>,
    /// Total number of dropped broadcast notifications due to lagged receivers.
    pub notification_lagged_total: Arc<AtomicU64>,
    /// Log lagged drops when the total crosses a multiple of this value.
    /// Set to 0 to disable lag logs while still counting drops.
    pub notification_lag_log_every: u64,
    /// Channel for server-push JSON-RPC notifications (stdio transport only).
    pub notify_tx: Option<crate::notify::NotifySender>,
    /// Whether `initialize` has been received (but `initialized` may not yet be set).
    /// Used to enforce the `initialize` → `initialized` ordering.
    pub initializing: Arc<AtomicBool>,
    /// Whether the client has completed the initialize/initialized handshake.
    pub initialized: Arc<AtomicBool>,
    /// Broadcast channel used to signal all active WebSocket connections to close gracefully.
    pub ws_shutdown_tx: broadcast::Sender<()>,
}

/// Intake services: external event sources and task completion handling.
pub struct IntakeServices {
    /// Feishu Bot intake handler. None when feishu intake is disabled or not configured.
    pub feishu_intake: Option<Arc<crate::intake::feishu::FeishuIntake>>,
    /// All GitHub issue pollers, one per configured repo. The same Arc instances
    /// are shared between the completion callback and the orchestrator so that
    /// `on_task_complete` (e.g. evicting a failed issue from `dispatched`)
    /// operates on the live poller rather than a detached clone.
    pub github_pollers: Vec<Arc<dyn crate::intake::IntakeSource>>,
    /// Completion callback invoked when a task reaches a terminal state.
    pub completion_callback: Option<task_runner::CompletionCallback>,
}

pub struct AppState {
    pub core: CoreServices,
    pub engines: EngineServices,
    pub observability: ObservabilityServices,
    pub concurrency: ConcurrencyServices,
    pub runtime_hosts: Arc<crate::runtime_hosts::RuntimeHostManager>,
    pub runtime_project_cache: Arc<crate::runtime_project_cache::RuntimeProjectCacheManager>,
    /// Serializes runtime snapshot writes to avoid out-of-order persistence.
    pub runtime_state_persist_lock: Mutex<()>,
    /// Set when a runtime-state persist fails; the next successful
    /// `persist_runtime_state` call clears it.  Handlers that find no
    /// in-memory mutation (e.g. idempotent deregister retry) still trigger a
    /// persist when this flag is set, converging durable state.
    pub runtime_state_dirty: AtomicBool,
    pub notifications: NotificationServices,
    pub intake: IntakeServices,
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    /// Subsystem names that degraded to `None` at startup (optional stores only).
    /// Set once during `build_app_state`; read-only thereafter.
    pub degraded_subsystems: Vec<&'static str>,

    // ── Service layer ────────────────────────────────────────────────────────
    // Trait-based abstractions for independent testability. Each service owns
    // its dependencies; the fields above are preserved for handlers that have
    // not yet been migrated to the service interfaces.
    /// Project registry operations and default-root lookup.
    pub project_svc: Arc<dyn crate::services::project::ProjectService>,
    /// Task lifecycle queries and stream subscriptions.
    pub task_svc: Arc<dyn crate::services::task::TaskService>,
    /// Task enqueue: project resolution, agent dispatch, concurrency, workspace.
    pub execution_svc: Arc<dyn crate::services::execution::ExecutionService>,
}

impl AppState {
    pub fn observe_notification_lag(&self, dropped: u64) -> u64 {
        let previous_total = self
            .notifications
            .notification_lagged_total
            .fetch_add(dropped, Ordering::Relaxed);
        let dropped_total = previous_total.saturating_add(dropped);
        let log_every = self.notifications.notification_lag_log_every;
        if log_every > 0 && previous_total / log_every < dropped_total / log_every {
            tracing::warn!(
                dropped_since_last_recv = dropped,
                dropped_total,
                log_every,
                "notification receiver lagged; dropped broadcast notifications"
            );
        }
        dropped_total
    }

    pub async fn persist_runtime_state(&self) -> anyhow::Result<()> {
        let _guard = self.runtime_state_persist_lock.lock().await;
        let Some(store) = self.core.runtime_state_store.as_ref() else {
            return Ok(());
        };
        let (hosts, leases) = self.runtime_hosts.snapshot_state();
        let project_caches = self.runtime_project_cache.snapshot_state();
        match store.persist_snapshot(hosts, leases, project_caches).await {
            Ok(()) => {
                self.runtime_state_dirty.store(false, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                self.runtime_state_dirty.store(true, Ordering::Release);
                Err(e)
            }
        }
    }

    /// Returns `true` when a previous persist failed and durable state may be
    /// stale.  Handlers that skip their own mutation (e.g. idempotent
    /// deregister retry returning NOT_FOUND) should check this and re-persist
    /// to converge.
    pub fn is_runtime_state_dirty(&self) -> bool {
        self.runtime_state_dirty.load(Ordering::Acquire)
    }
}
