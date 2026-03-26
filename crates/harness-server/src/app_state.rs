use crate::{server::HarnessServer, task_runner};
use dashmap::DashMap;
use harness_protocol::RpcNotification;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};

/// Per-identifier rate limiter for `POST /auth/reset-password`.
///
/// Uses a 1-hour rolling window per email address to prevent brute-force
/// and enumeration attacks on the password reset flow.
///
/// Memory is bounded: at most `max_tracked_keys` identifiers are tracked at
/// once; expired timestamps are evicted lazily on each access.
pub struct PasswordResetRateLimiter {
    timestamps: Mutex<HashMap<String, VecDeque<Instant>>>,
    max_per_hour: usize,
    max_tracked_keys: usize,
}

impl PasswordResetRateLimiter {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

    pub fn new(max_per_hour: u32) -> Self {
        Self {
            timestamps: Mutex::new(HashMap::new()),
            max_per_hour: max_per_hour as usize,
            max_tracked_keys: 100_000,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_cap(max_per_hour: u32, max_tracked_keys: usize) -> Self {
        Self {
            timestamps: Mutex::new(HashMap::new()),
            max_per_hour: max_per_hour as usize,
            max_tracked_keys,
        }
    }

    /// Returns `true` if the request is within the rate limit and increments the counter.
    pub fn check_and_increment(&self, identifier: &str) -> bool {
        let mut map = self.timestamps.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();

        // Evict timestamps outside the rolling window for this identifier.
        if let Some(entry) = map.get_mut(identifier) {
            while let Some(&front) = entry.front() {
                if now.duration_since(front) >= Self::WINDOW {
                    entry.pop_front();
                } else {
                    break;
                }
            }
            if entry.is_empty() {
                map.remove(identifier);
            }
        }

        // Reject new identifiers when map is at capacity (memory-DoS guard).
        if !map.contains_key(identifier) && map.len() >= self.max_tracked_keys {
            return false;
        }

        let entry = map.entry(identifier.to_string()).or_default();
        if entry.len() < self.max_per_hour {
            entry.push_back(now);
            true
        } else {
            false
        }
    }
}

/// Per-source rate limiter for `POST /signals` ingestion.
pub struct SignalRateLimiter {
    counts: Mutex<HashMap<String, (u32, Instant)>>,
    max_per_minute: u32,
}

impl SignalRateLimiter {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
            max_per_minute,
        }
    }

    /// Returns `true` if the request is within the rate limit and increments the counter.
    pub fn check_and_increment(&self, source: &str) -> bool {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let entry = counts.entry(source.to_string()).or_insert((0, now));
        if now.duration_since(entry.1) >= std::time::Duration::from_secs(60) {
            *entry = (1, now);
            true
        } else if entry.0 < self.max_per_minute {
            entry.0 += 1;
            true
        } else {
            false
        }
    }
}

/// Core services: thread/task management and persistence.
pub struct CoreServices {
    pub server: Arc<HarnessServer>,
    pub project_root: std::path::PathBuf,
    /// Home directory captured at startup to avoid TOCTOU when validating
    /// project roots against `$HOME` in concurrent requests.
    pub home_dir: std::path::PathBuf,
    pub tasks: Arc<task_runner::TaskStore>,
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub plan_db: Option<crate::plan_db::PlanDb>,
    /// In-memory plan cache hydrated from `plan_db` on startup.
    /// Write-through: every mutation must also persist via `plan_db`.
    pub plan_cache: Arc<DashMap<String, harness_exec::ExecPlan>>,
    pub project_registry: Option<std::sync::Arc<crate::project_registry::ProjectRegistry>>,
}

/// Engine services: skills, rules, and garbage collection.
pub struct EngineServices {
    pub skills: Arc<RwLock<harness_skills::SkillStore>>,
    pub rules: Arc<RwLock<harness_rules::engine::RuleEngine>>,
    pub gc_agent: Arc<harness_gc::GcAgent>,
}

/// Observability services: event store and telemetry.
pub struct ObservabilityServices {
    pub events: Arc<harness_observe::EventStore>,
    pub signal_rate_limiter: Arc<SignalRateLimiter>,
    pub password_reset_rate_limiter: Arc<PasswordResetRateLimiter>,
    pub review_store: Option<Arc<crate::review_store::ReviewStore>>,
}

/// Concurrency services: task queue and workspace isolation.
pub struct ConcurrencyServices {
    pub task_queue: Arc<crate::task_queue::TaskQueue>,
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
    /// Pre-built GitHub intake poller, shared between orchestrator and completion callback.
    pub github_intake: Option<Arc<dyn crate::intake::IntakeSource>>,
    /// Completion callback invoked when a task reaches a terminal state.
    pub completion_callback: Option<task_runner::CompletionCallback>,
}

pub struct AppState {
    pub core: CoreServices,
    pub engines: EngineServices,
    pub observability: ObservabilityServices,
    pub concurrency: ConcurrencyServices,
    pub notifications: NotificationServices,
    pub intake: IntakeServices,
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,

    // ── Service layer ────────────────────────────────────────────────────────
    // Trait-based abstractions for independent testability. Each service owns
    // its dependencies; the fields above are preserved for handlers that have
    // not yet been migrated to the service interfaces.
    /// Project registry operations and default-root lookup.
    pub project_svc: Arc<dyn crate::services::ProjectService>,
    /// Task lifecycle queries and stream subscriptions.
    pub task_svc: Arc<dyn crate::services::TaskService>,
    /// Task enqueue: project resolution, agent dispatch, concurrency, workspace.
    pub execution_svc: Arc<dyn crate::services::ExecutionService>,
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
}

/// Resolve the reviewer agent for independent agent review.
///
/// 1. If `config.reviewer_agent` is set and differs from implementor, use it.
/// 2. Otherwise, auto-select the first registered agent that isn't the implementor.
/// 3. If none found, return None (agent review will be skipped).
pub(crate) fn resolve_reviewer(
    registry: &harness_agents::AgentRegistry,
    config: &harness_core::AgentReviewConfig,
    implementor_name: &str,
) -> (
    Option<Arc<dyn harness_core::CodeAgent>>,
    harness_core::AgentReviewConfig,
) {
    if !config.enabled {
        return (None, config.clone());
    }

    // Explicit reviewer
    if !config.reviewer_agent.is_empty() {
        if config.reviewer_agent == implementor_name {
            tracing::warn!(
                "agents.review.reviewer_agent == implementor '{}', skipping agent review",
                implementor_name
            );
            return (None, config.clone());
        }
        if let Some(agent) = registry.get(&config.reviewer_agent) {
            return (Some(agent), config.clone());
        }
        tracing::warn!(
            "agents.review.reviewer_agent '{}' not registered, skipping agent review",
            config.reviewer_agent
        );
        return (None, config.clone());
    }

    // Auto-select: first agent != implementor
    for name in registry.list() {
        if name != implementor_name {
            if let Some(agent) = registry.get(name) {
                return (Some(agent), config.clone());
            }
        }
    }

    (None, config.clone())
}
