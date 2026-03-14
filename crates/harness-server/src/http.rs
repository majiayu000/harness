use crate::{router, server::HarnessServer, task_runner};
use anyhow::Context;
use axum::{
    body::Bytes,
    extract::DefaultBodyLimit,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use harness_protocol::{RpcNotification, RpcRequest};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};

const MAX_WEBHOOK_BODY_BYTES: usize = 512 * 1024;
const SIGNAL_RATE_LIMIT_PER_MINUTE: u32 = 100;

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

pub(crate) mod task_routes;

/// Core services: thread/task management and persistence.
pub struct CoreServices {
    pub server: Arc<HarnessServer>,
    pub project_root: std::path::PathBuf,
    pub tasks: Arc<task_runner::TaskStore>,
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub plan_db: Option<crate::plan_db::PlanDb>,
    pub plans:
        Arc<RwLock<std::collections::HashMap<harness_core::ExecPlanId, harness_exec::ExecPlan>>>,
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
    /// Whether the client has completed the initialize/initialized handshake.
    pub initialized: Arc<AtomicBool>,
    /// Broadcast channel used to signal all active WebSocket connections to close gracefully.
    pub ws_shutdown_tx: broadcast::Sender<()>,
}

pub struct AppState {
    pub core: CoreServices,
    pub engines: EngineServices,
    pub observability: ObservabilityServices,
    pub concurrency: ConcurrencyServices,
    pub notifications: NotificationServices,
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    /// Feishu Bot intake handler. None when feishu intake is disabled or not configured.
    pub feishu_intake: Option<Arc<crate::intake::feishu::FeishuIntake>>,
    /// Pre-built GitHub intake poller, shared between orchestrator and completion callback.
    pub github_intake: Option<Arc<dyn crate::intake::IntakeSource>>,
    /// Completion callback invoked when a task reaches a terminal state.
    pub completion_callback: Option<task_runner::CompletionCallback>,
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
fn resolve_project_root(configured_root: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let project_root = configured_root.canonicalize().map_err(|e| {
        anyhow::anyhow!(
            "invalid server.project_root '{}': {e}",
            configured_root.display()
        )
    })?;
    if !project_root.is_dir() {
        anyhow::bail!(
            "server.project_root is not a directory: {}",
            project_root.display()
        );
    }
    Ok(project_root)
}

/// Build an AppState with all stores. Used by both HTTP and stdio transports.
pub async fn build_app_state(server: Arc<HarnessServer>) -> anyhow::Result<AppState> {
    let dir = server.config.server.data_dir.clone();
    let project_root = resolve_project_root(&server.config.server.project_root)?;
    std::fs::create_dir_all(&dir)?;
    tracing::info!(
        data_dir = %dir.display(),
        project_root = %project_root.display(),
        discovery_paths = ?server.config.rules.discovery_paths,
        builtin_path = ?server.config.rules.builtin_path,
        exec_policy_paths = ?server.config.rules.exec_policy_paths,
        requirements_path = ?server.config.rules.requirements_path,
        session_renewal_secs = server.config.observe.session_renewal_secs,
        log_retention_days = server.config.observe.log_retention_days,
        "harness: effective config"
    );
    match server.config.server.github_webhook_secret.as_deref() {
        Some("") => {
            tracing::warn!(
                "server.github_webhook_secret is configured as empty string; refusing webhook requests until this is set to a non-empty value"
            );
        }
        None => {
            tracing::warn!(
                "server.github_webhook_secret is not configured; refusing webhook requests until this is set to a non-empty value"
            );
        }
        Some(_) => {}
    }

    let db_path = dir.join("tasks.db");
    tracing::info!("task db: {}", db_path.display());
    let tasks = task_runner::TaskStore::open(&db_path).await?;

    let mut rule_engine = harness_rules::engine::RuleEngine::new();
    rule_engine.configure_sources(
        server.config.rules.discovery_paths.clone(),
        server.config.rules.builtin_path.clone(),
        server.config.rules.requirements_path.clone(),
    );
    if let Err(e) = rule_engine.load_builtin() {
        tracing::warn!("failed to load builtin rules: {e}");
    }
    match rule_engine.auto_register_builtin_guards(&dir) {
        Ok(registered) => {
            tracing::info!(
                registered_guard_count = registered,
                total_guard_count = rule_engine.guards().len(),
                "rules: builtin guard auto-registration completed"
            );
        }
        Err(e) => {
            tracing::warn!("failed to auto-register builtin guards: {e}");
        }
    }
    match rule_engine.auto_register_project_guards(&project_root.join(".harness/guards")) {
        Ok(registered) => {
            tracing::info!(
                registered_guard_count = registered,
                total_guard_count = rule_engine.guards().len(),
                "rules: project guard auto-registration completed"
            );
        }
        Err(e) => {
            tracing::warn!("failed to auto-register project guards: {e}");
        }
    }
    rule_engine
        .load_exec_policy_files(&server.config.rules.exec_policy_paths)
        .context("failed to load rules.exec_policy_paths")?;
    rule_engine
        .load_configured_requirements()
        .context("failed to load configured rules.requirements_path")?;

    let events = Arc::new(
        harness_observe::EventStore::with_policies_and_otel(
            &dir,
            server.config.observe.session_renewal_secs,
            server.config.observe.log_retention_days,
            &server.config.otel,
        )
        .await?,
    );

    let signal_detector = harness_gc::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::ProjectId::new(),
    );
    let draft_store = harness_gc::DraftStore::new(&dir)?;
    let gc_agent = Arc::new(harness_gc::GcAgent::new(
        harness_core::GcConfig::default(),
        signal_detector,
        draft_store,
    ));

    let thread_db_path = dir.join("threads.db");
    let thread_db = crate::thread_db::ThreadDb::open(&thread_db_path).await?;
    let plan_db = crate::plan_db::PlanDb::open(&dir.join("plans.db")).await?;

    let project_registry =
        crate::project_registry::ProjectRegistry::open(&dir.join("projects.db")).await?;
    // Auto-register the default project from --project-root on startup.
    let default_project = crate::project_registry::Project {
        id: "default".to_string(),
        root: project_root.clone(),
        max_concurrent: None,
        default_agent: None,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = project_registry.register(default_project).await {
        tracing::warn!("failed to auto-register default project: {e}");
    } else {
        tracing::info!(
            project_root = %project_root.display(),
            "project registry: default project registered"
        );
    }
    let plans_md_dir = dir.join("plans");
    match plan_db.migrate_from_markdown_dir(&plans_md_dir).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(
            count = n,
            "plan migration: imported {} plan(s) from markdown",
            n
        ),
        Err(e) => tracing::warn!("plan migration: failed: {e}"),
    }
    let configured_capacity = server.config.server.notification_broadcast_capacity;
    let notification_broadcast_capacity = configured_capacity.max(1);
    let notification_lag_log_every = server.config.server.notification_lag_log_every;
    if configured_capacity == 0 {
        tracing::warn!(
            "server.notification_broadcast_capacity=0 is invalid; falling back to capacity=1"
        );
    }
    // Load persisted threads into the in-memory ThreadManager cache
    for thread in thread_db.list().await? {
        server
            .thread_manager
            .threads_cache()
            .insert(thread.id.as_str().to_string(), thread);
    }

    let mut skill_store = harness_skills::SkillStore::new()
        .with_persist_dir(dir.join("skills"))
        .with_discovery(&project_root);
    skill_store.load_builtin();
    if let Err(e) = skill_store.discover() {
        tracing::warn!("Failed to reload persisted skills on startup: {}", e);
    }

    let rules = Arc::new(RwLock::new(rule_engine));

    let validation_config = server.config.validation.clone();

    let workspace_mgr =
        match crate::workspace::WorkspaceManager::new(server.config.workspace.clone()) {
            Ok(mgr) => {
                tracing::info!(
                    root = %server.config.workspace.root.display(),
                    "workspace manager initialized"
                );
                Some(Arc::new(mgr))
            }
            Err(e) => {
                tracing::warn!(
                "failed to initialize workspace manager: {e}; running without workspace isolation"
            );
                None
            }
        };

    let task_queue = Arc::new(crate::task_queue::TaskQueue::new(
        &server.config.concurrency,
    ));
    tracing::info!(
        max_concurrent = server.config.concurrency.max_concurrent_tasks,
        max_queue_size = server.config.concurrency.max_queue_size,
        "task queue initialized"
    );

    let feishu_intake = server.config.intake.feishu.as_ref().and_then(|cfg| {
        if cfg.enabled {
            tracing::info!(
                trigger_keyword = %cfg.trigger_keyword,
                "intake: Feishu bot registered"
            );
            Some(Arc::new(crate::intake::feishu::FeishuIntake::new(
                cfg.clone(),
            )))
        } else {
            None
        }
    });

    let github_intake: Option<Arc<dyn crate::intake::IntakeSource>> = server
        .config
        .intake
        .github
        .as_ref()
        .filter(|cfg| cfg.enabled && !cfg.repo.is_empty())
        .map(|cfg| {
            Arc::new(crate::intake::github_issues::GitHubIssuesPoller::new(
                cfg,
                Some(&dir),
            )) as Arc<dyn crate::intake::IntakeSource>
        });

    let quality_trigger = {
        let gc_cfg = &server.config.gc;
        Arc::new(crate::quality_trigger::QualityTrigger::new(
            events.clone(),
            gc_agent.clone(),
            server.agent_registry.clone(),
            project_root.clone(),
            gc_cfg.auto_gc_grades.clone(),
            gc_cfg.auto_gc_cooldown_secs,
        ))
    };

    let completion_callback = build_completion_callback(
        &feishu_intake,
        &github_intake,
        server.config.agents.review.clone(),
        Some(quality_trigger),
    );

    let hook_enforcement = server.config.rules.hook_enforcement;
    let events_for_hooks = events.clone();

    Ok(AppState {
        core: CoreServices {
            server,
            project_root,
            tasks,
            thread_db: Some(thread_db),
            plans: {
                let mut map = std::collections::HashMap::new();
                match plan_db.list().await {
                    Ok(persisted) => {
                        for plan in persisted {
                            map.insert(plan.id.clone(), plan);
                        }
                    }
                    Err(e) => tracing::warn!("failed to load persisted plans on startup: {e}"),
                }
                Arc::new(RwLock::new(map))
            },
            plan_db: Some(plan_db),
            project_registry: Some(project_registry),
        },
        engines: EngineServices {
            skills: Arc::new(RwLock::new(skill_store)),
            rules: rules.clone(),
            gc_agent,
        },
        observability: ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(SignalRateLimiter::new(SIGNAL_RATE_LIMIT_PER_MINUTE)),
        },
        concurrency: ConcurrencyServices {
            task_queue,
            workspace_mgr,
        },
        notifications: NotificationServices {
            notification_tx: broadcast::channel(notification_broadcast_capacity).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every,
            notify_tx: None,
            initialized: Arc::new(AtomicBool::new(false)),
            ws_shutdown_tx: broadcast::channel(1).0,
        },
        interceptors: vec![
            Arc::new(crate::contract_validator::ContractValidator::new()),
            Arc::new(crate::rule_enforcer::RuleEnforcer::new(rules.clone())),
            Arc::new(crate::hook_enforcer::HookEnforcer::new(
                rules,
                events_for_hooks,
                hook_enforcement,
            )),
            Arc::new(crate::post_validator::PostExecutionValidator::new(
                validation_config,
            )),
        ],
        feishu_intake,
        github_intake,
        completion_callback,
    })
}

fn build_completion_callback(
    feishu_intake: &Option<Arc<crate::intake::feishu::FeishuIntake>>,
    github_intake: &Option<Arc<dyn crate::intake::IntakeSource>>,
    review_config: harness_core::AgentReviewConfig,
    quality_trigger: Option<Arc<crate::quality_trigger::QualityTrigger>>,
) -> Option<task_runner::CompletionCallback> {
    let mut sources: std::collections::HashMap<String, Arc<dyn crate::intake::IntakeSource>> =
        std::collections::HashMap::new();
    if let Some(gh) = github_intake {
        sources.insert(gh.name().to_string(), gh.clone());
    }
    if let Some(fi) = feishu_intake {
        let fi_source: Arc<dyn crate::intake::IntakeSource> = fi.clone();
        sources.insert(fi_source.name().to_string(), fi_source);
    }
    if sources.is_empty() && !review_config.review_bot_auto_trigger && quality_trigger.is_none() {
        return None;
    }
    let sources = Arc::new(sources);
    Some(Arc::new(move |task: task_runner::TaskState| {
        let sources = sources.clone();
        let review_config = review_config.clone();
        let quality_trigger = quality_trigger.clone();
        Box::pin(async move {
            // Grade recent events and auto-trigger GC if quality is poor.
            if let Some(qt) = quality_trigger {
                qt.check_and_maybe_trigger().await;
            }

            // Auto-trigger review bot comment when task completes with a PR URL.
            if review_config.review_bot_auto_trigger {
                if let task_runner::TaskStatus::Done = &task.status {
                    if let Some(pr_url) = task.pr_url.as_deref() {
                        if let Some((owner, repo, pr_num)) =
                            harness_core::prompts::parse_github_pr_url(pr_url)
                        {
                            match std::env::var("GITHUB_TOKEN") {
                                Ok(token) if !token.is_empty() => {
                                    if let Err(e) = post_review_bot_comment(
                                        &owner,
                                        &repo,
                                        pr_num,
                                        &review_config.review_bot_command,
                                        &token,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            pr_url,
                                            "review_bot_auto_trigger: failed to post comment: {e}"
                                        );
                                    } else {
                                        tracing::info!(
                                            pr_url,
                                            comment = review_config.review_bot_command,
                                            "review bot comment posted"
                                        );
                                    }
                                }
                                _ => {
                                    tracing::warn!(
                                        pr_url,
                                        "review_bot_auto_trigger: GITHUB_TOKEN not set or empty; skipping"
                                    );
                                }
                            }
                        }
                    }
                }
            }

            // Intake source notification.
            let Some(source_name) = task.source.as_deref() else {
                return;
            };
            let Some(external_id) = task.external_id.as_deref() else {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    "completion_callback: task missing external_id, skipping"
                );
                return;
            };
            let Some(source) = sources.get(source_name) else {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    "completion_callback: intake source not found, skipping"
                );
                return;
            };
            let summary = match &task.status {
                task_runner::TaskStatus::Done => task
                    .pr_url
                    .as_deref()
                    .map(|url| format!("PR: {url}"))
                    .unwrap_or_else(|| "Task completed.".to_string()),
                task_runner::TaskStatus::Failed => {
                    task.error.as_deref().unwrap_or("unknown error").to_string()
                }
                _ => {
                    tracing::warn!(
                        task_id = ?task.id,
                        status = ?task.status,
                        "completion_callback: called with non-terminal status, skipping"
                    );
                    return;
                }
            };
            let result = crate::intake::TaskCompletionResult {
                status: task.status.clone(),
                pr_url: task.pr_url.clone(),
                error: task.error.clone(),
                summary,
            };
            if let Err(e) = source.on_task_complete(external_id, &result).await {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    "on_task_complete failed: {e}"
                );
            }
        })
    }))
}

/// Post a comment to a GitHub PR via the Issues API.
async fn post_review_bot_comment(
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
    github_token: &str,
) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {github_token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "harness-bot")
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {status}: {text}");
    }
    Ok(())
}

/// Resolve the reviewer agent for independent agent review.
///
/// 1. If `config.reviewer_agent` is set and differs from implementor, use it.
/// 2. Otherwise, auto-select the first registered agent that isn't the implementor.
/// 3. If none found, return None (agent review will be skipped).
fn resolve_reviewer(
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

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");

    let state = Arc::new(build_app_state(server).await?);

    let initial_grade = {
        let events = state
            .observability
            .events
            .query(&harness_core::EventFilters::default())
            .await
            .unwrap_or_default();
        // Use violations from the most recent scan (identified by the latest rule_scan session_id)
        // rather than all historical rule_check events, to avoid permanently depressing the grade.
        let violation_count = events
            .iter()
            .rev()
            .find(|e| e.hook == "rule_scan")
            .map(|scan| {
                events
                    .iter()
                    .filter(|e| e.hook == "rule_check" && e.session_id == scan.session_id)
                    .count()
            })
            .unwrap_or(0);
        harness_observe::quality::QualityGrader::grade(&events, violation_count).grade
    };
    crate::scheduler::Scheduler::from_grade(initial_grade).start(state.clone());
    crate::intake::build_orchestrator(
        &state.core.server.config.intake,
        Some(&state.core.server.config.server.data_dir),
        state.feishu_intake.clone(),
    )
    .start(state.clone());

    let app = Router::new()
        .route("/", get(crate::dashboard::index))
        .route("/favicon.ico", get(crate::dashboard::favicon))
        .route("/health", get(health_check))
        .route("/rpc", post(handle_rpc))
        .route("/ws", get(crate::websocket::ws_handler))
        .route("/tasks", post(task_routes::create_task))
        .route("/tasks", get(list_tasks))
        .route("/tasks/batch", post(task_routes::create_tasks_batch))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/stream", get(stream_task_sse))
        .route(
            "/projects",
            post(crate::handlers::projects::register_project)
                .get(crate::handlers::projects::list_projects),
        )
        .route(
            "/projects/{id}",
            get(crate::handlers::projects::get_project)
                .delete(crate::handlers::projects::delete_project),
        )
        .route("/projects/queue-stats", get(project_queue_stats))
        .route("/api/intake", get(intake_status))
        .route(
            "/webhook",
            post(github_webhook).layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES)),
        )
        .route(
            "/webhook/feishu",
            post(crate::intake::feishu::feishu_webhook)
                .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES)),
        )
        .route(
            "/signals",
            post(ingest_signal).layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_BYTES)),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let ws_shutdown_tx = state.notifications.ws_shutdown_tx.clone();
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("server shutting down: closing WebSocket connections");
            ws_shutdown_tx.send(()).ok();
        })
        .await;
    tracing::info!("server shutting down");
    state.observability.events.shutdown().await;
    serve_result?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.core.tasks.count();
    Json(json!({"status": "ok", "tasks": count}))
}

/// GET /projects — per-project queue stats alongside the global queue summary.
async fn project_queue_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let tq = &state.concurrency.task_queue;
    let projects: serde_json::Map<String, serde_json::Value> = tq
        .all_project_stats()
        .into_iter()
        .map(|(id, s)| {
            (
                id,
                json!({
                    "running": s.running,
                    "queued": s.queued,
                    "limit": s.limit,
                }),
            )
        })
        .collect();
    Json(json!({
        "global": {
            "running": tq.running_count(),
            "queued": tq.queued_count(),
            "limit": tq.global_limit(),
        },
        "projects": projects,
    }))
}

async fn handle_rpc(State(state): State<Arc<AppState>>, Json(req): Json<RpcRequest>) -> Response {
    match router::handle_request(&state, req).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let secret = match state
        .core
        .server
        .config
        .server
        .github_webhook_secret
        .as_deref()
    {
        Some("") => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "invalid server.github_webhook_secret configuration"})),
            )
        }
        Some(secret) => secret,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "missing server.github_webhook_secret configuration"})),
            )
        }
    };
    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
    {
        Some(signature) => signature,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing header x-hub-signature-256"})),
            )
        }
    };
    if !crate::webhook::verify_github_signature(secret, signature, body.as_ref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid webhook signature"})),
        );
    }

    let event = match headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
    {
        Some(event) => event,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing header x-github-event"})),
            )
        }
    };
    if !crate::webhook::is_valid_github_event_name(event) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid header x-github-event"})),
        );
    }

    let (request, reason) =
        match crate::webhook::parse_github_webhook_task_request(event, body.as_ref()) {
            Ok(parsed) => parsed,
            Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
        };

    let Some(mut req) = request else {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "ignored",
                "reason": reason,
            })),
        );
    };

    if req.project.is_none() {
        req.project = Some(state.core.project_root.clone());
    }

    match task_routes::enqueue_task(&state, req).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "accepted",
                "reason": reason,
                "task_id": task_id.0,
            })),
        ),
        Err(task_routes::EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(task_routes::EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn list_tasks(State(state): State<Arc<AppState>>) -> Json<Vec<task_runner::TaskSummary>> {
    let tasks = state
        .core
        .tasks
        .list_all()
        .into_iter()
        .map(|t| t.summary())
        .collect();
    Json(tasks)
}

async fn get_task(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.core.tasks.get(&task_runner::TaskId(id)) {
        Some(task) => Json(task).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
    }
}

/// GET /tasks/{id}/stream — real-time SSE stream of agent execution events.
///
/// Subscribes to the task's broadcast channel and forwards each [`StreamItem`]
/// as a JSON-encoded SSE data event. The stream ends when the task completes
/// (channel closed). If the receiver lags, a synthetic "lag" event is emitted
/// noting how many events were dropped; streaming then continues.
async fn stream_task_sse(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let task_id = task_runner::TaskId(id);

    let rx = match state.core.tasks.subscribe_task_stream(&task_id) {
        Some(rx) => rx,
        None => {
            if state.core.tasks.get(&task_id).is_none() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "task not found"})),
                )
                    .into_response();
            }
            // Task exists but stream already closed (task completed before client connected).
            let stream = futures::stream::empty::<Result<Event, std::convert::Infallible>>();
            return Sse::new(stream).into_response();
        }
    };

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(item) => {
                let data = serde_json::to_string(&item).unwrap_or_default();
                Some((
                    Ok::<Event, std::convert::Infallible>(Event::default().data(data)),
                    rx,
                ))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                let event = Event::default()
                    .event("lag")
                    .data(format!("dropped {n} events due to slow consumer"));
                Some((Ok(event), rx))
            }
        }
    });

    Sse::new(stream).into_response()
}

/// GET /api/intake — current status of all intake channels and recent dispatches.
async fn intake_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let intake_config = &state.core.server.config.intake;
    let all_tasks = state.core.tasks.list_all();

    let github_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            t.source.as_deref() == Some("github")
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let feishu_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            t.source.as_deref() == Some("feishu")
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let dashboard_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            (t.source.as_deref() == Some("dashboard") || t.source.is_none())
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let github_channel = json!({
        "name": "github",
        "enabled": intake_config.github.as_ref().map(|c| c.enabled).unwrap_or(false),
        "repo": intake_config.github.as_ref().map(|c| c.repo.as_str()).unwrap_or(""),
        "active": github_active,
    });

    let feishu_channel = json!({
        "name": "feishu",
        "enabled": intake_config.feishu.as_ref().map(|c| c.enabled).unwrap_or(false),
        "keyword": intake_config.feishu.as_ref().map(|c| c.trigger_keyword.as_str()).unwrap_or(""),
        "active": feishu_active,
    });

    let dashboard_channel = json!({
        "name": "dashboard",
        "enabled": true,
        "active": dashboard_active,
    });

    let mut recent_dispatches: Vec<serde_json::Value> = all_tasks
        .iter()
        .filter(|t| t.source.is_some())
        .map(|t| {
            json!({
                "source": t.source,
                "external_id": t.external_id,
                "task_id": t.id.0,
                "status": serde_json::to_value(&t.status).unwrap_or(json!("unknown")),
                "pr_url": t.pr_url,
            })
        })
        .collect();
    recent_dispatches.truncate(10);

    Json(json!({
        "channels": [github_channel, feishu_channel, dashboard_channel],
        "recent_dispatches": recent_dispatches,
    }))
}

/// Request body for `POST /signals`.
#[derive(serde::Deserialize)]
struct IngestSignalRequest {
    source: String,
    #[serde(default)]
    severity: Option<harness_core::Severity>,
    payload: serde_json::Value,
}

/// Infer severity from a GitHub webhook payload: CI failure → High, changes_requested → Medium.
fn infer_github_severity(payload: &serde_json::Value) -> Option<harness_core::Severity> {
    if let Some(obj) = payload.as_object() {
        // check_run completed with failure
        if let (Some(action), Some(check_run)) = (
            obj.get("action").and_then(|v| v.as_str()),
            obj.get("check_run"),
        ) {
            if action == "completed" {
                let conclusion = check_run
                    .get("conclusion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if conclusion == "failure" {
                    return Some(harness_core::Severity::High);
                }
            }
        }
        // pull_request_review with changes_requested
        if let Some(review) = obj.get("review") {
            let state = review.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state.eq_ignore_ascii_case("changes_requested") {
                return Some(harness_core::Severity::Medium);
            }
        }
    }
    None
}

/// POST /signals — ingest an external signal (CI failure, review feedback, etc.).
///
/// Validates the `x-hub-signature-256` HMAC-SHA256 header using the configured
/// `server.github_webhook_secret`. Rate-limited to 100 requests per source per minute.
async fn ingest_signal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let secret = match state
        .core
        .server
        .config
        .server
        .github_webhook_secret
        .as_deref()
    {
        Some("") | None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server.github_webhook_secret not configured"})),
            )
        }
        Some(s) => s,
    };

    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(sig) => sig,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing header x-hub-signature-256"})),
            )
        }
    };

    if !crate::webhook::verify_github_signature(secret, signature, body.as_ref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid webhook signature"})),
        );
    }

    let req: IngestSignalRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid payload: {e}")})),
            )
        }
    };

    if !state
        .observability
        .signal_rate_limiter
        .check_and_increment(&req.source)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "rate limit exceeded: max 100 signals per minute per source"})),
        );
    }

    let severity = req.severity.unwrap_or_else(|| {
        if req.source == "github" {
            infer_github_severity(&req.payload).unwrap_or(harness_core::Severity::Low)
        } else {
            harness_core::Severity::Low
        }
    });

    let signal =
        harness_core::ExternalSignal::new(req.source.clone(), severity, req.payload.clone());
    let signal_id = signal.id.clone();

    if let Err(e) = state.observability.events.log_external_signal(&signal) {
        tracing::error!(source = %req.source, "failed to store external signal: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to store signal"})),
        );
    }

    tracing::info!(
        source = %req.source,
        severity = ?severity,
        signal_id = %signal_id,
        "external signal ingested"
    );

    (
        StatusCode::OK,
        Json(json!({"status": "accepted", "id": signal_id.as_str()})),
    )
}

#[cfg(test)]
mod startup_tests {
    use super::build_app_state;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::HarnessConfig;
    use std::sync::Arc;

    #[tokio::test]
    async fn build_app_state_auto_registers_builtin_guard() -> anyhow::Result<()> {
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root;
        config.server.data_dir = sandbox.path().join("data");

        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let state = build_app_state(server).await?;
        let rules = state.engines.rules.read().await;

        assert!(
            rules
                .guards()
                .iter()
                .any(|guard| guard.id.as_str() == harness_rules::engine::BUILTIN_BASELINE_GUARD_ID),
            "expected build_app_state to auto-register builtin guard"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
