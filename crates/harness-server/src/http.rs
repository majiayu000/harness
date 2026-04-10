use crate::{router, server::HarnessServer, task_runner};
use anyhow::Context;
use axum::{
    body::Bytes,
    extract::DefaultBodyLimit,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    middleware::{self},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use harness_protocol::methods::RpcRequest;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) mod auth;
pub(crate) mod rate_limit;
pub(crate) mod startup;
pub(crate) mod state;
pub(crate) mod task_routes;

pub(crate) use startup::{
    build_completion_callback, expand_tilde, infer_github_severity, resolve_project_root,
    IngestSignalRequest, PasswordResetRequest,
};
pub use state::{
    AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
    NotificationServices, ObservabilityServices,
};

/// Build an AppState with all stores. Used by both HTTP and stdio transports.
pub async fn build_app_state(server: Arc<HarnessServer>) -> anyhow::Result<AppState> {
    let dir = expand_tilde(&server.config.server.data_dir);
    let project_root = resolve_project_root(&server.config.server.project_root)?;
    std::fs::create_dir_all(&dir)?;
    // On Unix, verify that `dir` is a real directory and not a symbolic link.
    // A low-privileged attacker can pre-create a symlink at the fallback temp
    // path before a privileged harness process starts, redirecting all
    // persistent state (tasks.db, threads.db, events, workspaces) to an
    // attacker-controlled location.  Refusing to operate on a symlink is the
    // minimal safe response; production deployments must set `data_dir`
    // explicitly so the fallback path is never reached.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::symlink_metadata(&dir)
            .with_context(|| format!("failed to stat data_dir {:?}", dir))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "data_dir {:?} is a symbolic link; refusing to start to prevent \
                 potential symlink hijacking. Set an explicit `data_dir` in your \
                 harness config file.",
                dir
            );
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set 0o700 permissions on data_dir {:?}", dir))?;
    }
    tracing::debug!(
        data_dir = %dir.display(),
        project_root = %project_root.display(),
        discovery_paths = ?server.config.rules.discovery_paths,
        builtin_path = ?server.config.rules.builtin_path,
        exec_policy_paths = ?server.config.rules.exec_policy_paths,
        requirements_path = ?server.config.rules.requirements_path,
        session_renewal_secs = server.config.observe.session_renewal_secs,
        log_retention_days = server.config.observe.log_retention_days,
        "config details (use RUST_LOG=debug to see)"
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

    let db_path = harness_core::config::dirs::default_db_path(&dir, "tasks");
    tracing::debug!("task db: {}", db_path.display());
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
            tracing::debug!(
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
            tracing::debug!(
                registered_guard_count = registered,
                total_guard_count = rule_engine.guards().len(),
                "rules: project guard auto-registration completed"
            );
        }
        Err(e) => {
            tracing::warn!("failed to auto-register project guards: {e}");
        }
    }
    // Load guards from each named startup project so non-default projects are
    // not silently unprotected.
    for (name, path) in &server.startup_projects {
        match rule_engine.auto_register_project_guards(&path.join(".harness/guards")) {
            Ok(registered) => {
                tracing::debug!(
                    project = %name,
                    registered_guard_count = registered,
                    total_guard_count = rule_engine.guards().len(),
                    "rules: startup project guard auto-registration completed"
                );
            }
            Err(e) => {
                tracing::warn!(project = %name, "failed to auto-register startup project guards: {e}");
            }
        }
    }
    rule_engine
        .load_exec_policy_files(&server.config.rules.exec_policy_paths)
        .context("failed to load rules.exec_policy_paths")?;
    rule_engine
        .load_configured_requirements()
        .context("failed to load configured rules.requirements_path")?;

    let events = Arc::new(
        harness_observe::event_store::EventStore::with_policies_and_otel(
            &dir,
            server.config.observe.session_renewal_secs,
            server.config.observe.log_retention_days,
            &server.config.otel,
        )
        .await?,
    );

    let signal_detector = harness_gc::signal_detector::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::types::ProjectId::from_str(
            project_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("default"),
        ),
    );
    let draft_store = harness_gc::draft_store::DraftStore::new(&dir)?;
    let gc_agent = Arc::new(
        harness_gc::gc_agent::GcAgent::new(
            server.config.gc.clone(),
            signal_detector,
            draft_store,
            project_root.clone(),
        )
        .with_checkpoint(dir.join("gc-checkpoint.json")),
    );

    let thread_db_path = harness_core::config::dirs::default_db_path(&dir, "threads");
    let thread_db = crate::thread_db::ThreadDb::open(&thread_db_path).await?;
    let plan_db =
        crate::plan_db::PlanDb::open(&harness_core::config::dirs::default_db_path(&dir, "plans"))
            .await?;

    let project_registry = crate::project_registry::ProjectRegistry::open(
        &harness_core::config::dirs::default_db_path(&dir, "projects"),
    )
    .await?;
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
    }
    // Register any extra named projects supplied via --project CLI flags.
    for (name, path) in &server.startup_projects {
        let proj = crate::project_registry::Project {
            id: name.clone(),
            root: path.clone(),
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = project_registry.register(proj).await {
            tracing::warn!(project = %name, "failed to register startup project: {e}");
        }
    }
    let plans_md_dir = dir.join("plans");
    match plan_db.migrate_from_markdown_dir(&plans_md_dir).await {
        Ok(0) => {}
        Ok(n) => tracing::debug!(
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

    // Load persisted plans into the in-memory plan cache
    let plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>> = Arc::new(DashMap::new());
    match plan_db.list().await {
        Ok(plans) => {
            let count = plans.len();
            for plan in plans {
                plan_cache.insert(plan.id.as_str().to_string(), plan);
            }
            if count > 0 {
                tracing::debug!(count, "plan cache: loaded {} plan(s) from db", count);
            }
        }
        Err(e) => tracing::warn!("plan cache: failed to load plans on startup: {e}"),
    }

    let mut skill_store = harness_skills::store::SkillStore::new()
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
                tracing::debug!(
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

    // Cleanup orphan worktrees from any previous crash.
    if let Some(ref wmgr) = workspace_mgr {
        let terminal_ids: Vec<crate::task_runner::TaskId> = tasks
            .list_all()
            .into_iter()
            .filter(|t| {
                matches!(
                    t.status,
                    crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
                )
            })
            .map(|t| t.id)
            .collect();
        wmgr.cleanup_orphan_worktrees(&project_root, &terminal_ids)
            .await;
    }

    let memory_pressure =
        server
            .config
            .concurrency
            .memory_pressure_threshold_mb
            .map(|threshold_mb| {
                let poll_secs = server.config.concurrency.memory_poll_interval_secs;
                tracing::info!(threshold_mb, poll_secs, "memory pressure monitor enabled");
                crate::memory_monitor::start(threshold_mb, poll_secs)
            });
    let task_queue = Arc::new(crate::task_queue::TaskQueue::new_with_pressure(
        &server.config.concurrency,
        memory_pressure,
    ));
    tracing::debug!(
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

    // Build ALL GitHub pollers once. The same Arc instances are shared between
    // the completion callback and the orchestrator, so on_task_complete operates
    // on the live poller's dispatched map rather than a detached clone.
    // Keyed as "github:{owner/repo}" for per-repo routing in the callback;
    // a "github" fallback entry (first poller) supports tasks persisted before
    // this multi-repo routing was introduced.
    let github_pollers: Vec<(String, Arc<dyn crate::intake::IntakeSource>)> = server
        .config
        .intake
        .github
        .as_ref()
        .filter(|cfg| cfg.enabled)
        .map(|cfg| {
            cfg.effective_repos()
                .into_iter()
                .map(|repo_cfg| {
                    tracing::info!(
                        repo = %repo_cfg.repo,
                        label = %repo_cfg.label,
                        "intake: GitHub Issues poller registered"
                    );
                    let key = format!("github:{}", repo_cfg.repo);
                    let poller = Arc::new(crate::intake::github_issues::GitHubIssuesPoller::new(
                        &repo_cfg,
                        Some(&dir),
                    )) as Arc<dyn crate::intake::IntakeSource>;
                    (key, poller)
                })
                .collect()
        })
        .unwrap_or_default();

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
        &github_pollers,
        server.config.agents.review.clone(),
        Some(quality_trigger),
        server.config.server.github_token.clone(),
    );

    // Validate recovered pending tasks in the background so startup is not blocked
    // by serial `gh pr view` calls. The completion_callback is passed so that tasks
    // marked Failed (closed PR) trigger intake cleanup (e.g. removing the issue from
    // the dispatched map so it can be re-dispatched on the next poll cycle).
    {
        let tasks_for_recovery = tasks.clone();
        let cb_for_recovery = completion_callback.clone();
        tokio::spawn(async move {
            tasks_for_recovery
                .validate_recovered_tasks(cb_for_recovery)
                .await;
        });
    }

    let hook_enforcement = server.config.rules.hook_enforcement;
    let events_for_hooks = events.clone();

    // ── Service layer construction ────────────────────────────────────────────
    // Build the interceptor list once so it can be shared with ExecutionService
    // without reconstructing it inside the AppState literal.
    let skills_arc = Arc::new(RwLock::new(skill_store));
    let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> = vec![
        Arc::new(crate::contract_validator::ContractValidator::new()),
        Arc::new(crate::rule_enforcer::RuleEnforcer::new(rules.clone())),
        Arc::new(crate::hook_enforcer::HookEnforcer::new(
            rules.clone(),
            events_for_hooks,
            hook_enforcement,
        )),
        Arc::new(crate::post_validator::PostExecutionValidator::new(
            validation_config,
        )),
    ];

    let project_svc = crate::services::project::DefaultProjectService::new(
        project_registry.clone(),
        project_root.clone(),
    );
    let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        skills_arc.clone(),
        events.clone(),
        interceptors.clone(),
        workspace_mgr.clone(),
        task_queue.clone(),
        completion_callback.clone(),
        Some(project_registry.clone()),
        server.config.server.allowed_project_roots.clone(),
    );

    // Spawn background watcher for AwaitingDeps tasks.
    {
        let store = tasks.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let ready_ids = crate::task_runner::check_awaiting_deps(&store);
                for task_id in ready_ids {
                    if let Err(e) = store.persist(&task_id).await {
                        tracing::warn!(
                            "dep-watcher: failed to persist {} after transition: {e}",
                            task_id.0
                        );
                    }
                }
            }
        });
    }

    let runtime_hosts = Arc::new(crate::runtime_hosts::RuntimeHostManager::new());
    let runtime_project_cache =
        Arc::new(crate::runtime_project_cache::RuntimeProjectCacheManager::new());
    let runtime_state_store = {
        let runtime_state_db_path =
            harness_core::config::dirs::default_db_path(&dir, "runtime_state");
        match crate::runtime_state_store::RuntimeStateStore::open(&runtime_state_db_path).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(
                    path = %runtime_state_db_path.display(),
                    "runtime state store init failed, runtime host state will not persist: {e}"
                );
                None
            }
        }
    };
    if let Some(store) = runtime_state_store.as_ref() {
        match store.load_snapshot().await {
            Ok(Some(snapshot)) => {
                let restored_hosts = runtime_hosts.restore_state(snapshot.hosts, snapshot.leases);
                let restored_project_caches =
                    runtime_project_cache.restore_state(snapshot.project_caches);
                tracing::info!(
                    restored_hosts = restored_hosts.0,
                    restored_leases = restored_hosts.1,
                    restored_project_caches,
                    "runtime state restored from persistent snapshot"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("failed to load runtime state snapshot on startup: {e}");
            }
        }
    }

    let signal_rate_limit = server.config.server.signal_rate_limit_per_minute;
    let password_reset_rate_limit = server.config.server.password_reset_rate_limit_per_hour;
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| project_root.clone());
    Ok(AppState {
        core: CoreServices {
            server,
            project_root,
            home_dir,
            tasks,
            thread_db: Some(thread_db),
            plan_db: Some(plan_db),
            plan_cache,
            project_registry: Some(project_registry),
            runtime_state_store,
        },
        engines: EngineServices {
            skills: skills_arc,
            rules,
            gc_agent,
        },
        observability: ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(rate_limit::SignalRateLimiter::new(signal_rate_limit)),
            password_reset_rate_limiter: Arc::new(rate_limit::PasswordResetRateLimiter::new(
                password_reset_rate_limit,
            )),
            review_store: {
                let review_db_path = harness_core::config::dirs::default_db_path(&dir, "reviews");
                match crate::review_store::ReviewStore::open(&review_db_path).await {
                    Ok(store) => Some(Arc::new(store)),
                    Err(e) => {
                        tracing::warn!(
                            "review store init failed, reviews will not be persisted: {e}"
                        );
                        None
                    }
                }
            },
        },
        concurrency: ConcurrencyServices {
            task_queue,
            workspace_mgr,
        },
        runtime_hosts,
        runtime_project_cache,
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
        notifications: state::NotificationServices {
            notification_tx: tokio::sync::broadcast::channel(notification_broadcast_capacity).0,
            notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notification_lag_log_every,
            notify_tx: None,
            initializing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors,
        intake: IntakeServices {
            feishu_intake,
            github_pollers: github_pollers.into_iter().map(|(_, p)| p).collect(),
            completion_callback,
        },
        project_svc,
        task_svc,
        execution_svc,
    })
}

/// Resolve the reviewer agent for independent agent review.
///
/// 1. If `config.reviewer_agent` is set and differs from implementor, use it.
/// 2. Otherwise, auto-select the first registered agent that isn't the implementor.
/// 3. If none found, return None (agent review will be skipped).
pub(crate) fn resolve_reviewer(
    registry: &harness_agents::registry::AgentRegistry,
    config: &harness_core::config::agents::AgentReviewConfig,
    implementor_name: &str,
) -> (
    Option<Arc<dyn harness_core::agent::CodeAgent>>,
    harness_core::config::agents::AgentReviewConfig,
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

/// Extract the PR number from a GitHub PR URL.
///
/// Handles:
/// - `.../pull/42`
/// - `.../pull/42/files`
/// - `.../pull/42#discussion_r...`
pub(crate) fn parse_pr_num_from_url(url: &str) -> Option<u64> {
    // Strip fragment first, then query string
    let url = url.split('#').next().unwrap_or(url);
    let url = url.split('?').next().unwrap_or(url);
    // Walk path segments looking for "pull", then parse the segment that follows
    let mut parts = url.split('/');
    while let Some(seg) = parts.next() {
        if seg == "pull" {
            return parts.next()?.parse::<u64>().ok();
        }
    }
    None
}

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");
    // Record true server start time before accepting any connections.
    crate::handlers::dashboard::SERVER_START.get_or_init(std::time::Instant::now);

    let state = Arc::new(build_app_state(server.clone()).await?);

    // Startup summary — one clean line instead of scattered logs.
    {
        let guard_count = state.engines.rules.read().await.guards().len();
        let skill_count = state.engines.skills.read().await.list().len();
        let task_count = state.core.tasks.list_all().len();
        tracing::info!(
            project = %state.core.project_root.display(),
            guards = guard_count,
            skills = skill_count,
            pending_tasks = task_count,
            "harness: ready"
        );
    }

    // Re-dispatch tasks that were recovered to pending after server restart.
    // These had PRs when the server crashed and need their review loop re-started.
    // Without this, recovered tasks silently hang in pending forever.
    //
    // Each task is re-dispatched in a background tokio task so that permit
    // acquisition never blocks serve() — if more tasks exist than available
    // concurrency slots, the background futures will simply wait in queue.
    {
        let recovered: Vec<_> = state
            .core
            .tasks
            .list_all()
            .into_iter()
            .filter(|t| matches!(t.status, task_runner::TaskStatus::Pending) && t.pr_url.is_some())
            .collect();
        if !recovered.is_empty() {
            tracing::info!(
                count = recovered.len(),
                "startup: re-dispatching recovered pending task(s) with PRs"
            );
            for task in recovered {
                let state = state.clone();
                tokio::spawn(async move {
                    let pr_url = task.pr_url.as_deref().unwrap_or("");
                    // Issue 4: robust parsing handles /pull/42/files and #fragment suffixes
                    let pr_num = match parse_pr_num_from_url(pr_url) {
                        Some(n) => n,
                        None => {
                            // pr_url is present but unparseable (empty, corrupted, or
                            // non-standard).  Simply returning would leave the task stuck in
                            // 'pending' forever — fail-close it instead so operators can see
                            // it and the re-dispatch filter never picks it up again.
                            tracing::error!(
                                task_id = ?task.id,
                                pr_url,
                                "startup recovery: cannot parse PR number from URL — marking task failed"
                            );
                            let bad_url = pr_url.to_owned();
                            if let Err(e) = task_runner::mutate_and_persist(
                                &state.core.tasks,
                                &task.id,
                                move |s| {
                                    s.status = task_runner::TaskStatus::Failed;
                                    s.error = Some(format!(
                                        "startup recovery: unparseable pr_url: {bad_url}"
                                    ));
                                },
                            )
                            .await
                            {
                                tracing::error!(
                                    task_id = ?task.id,
                                    "startup recovery: failed to persist failed status: {e}"
                                );
                            }
                            // Fire completion callback so intake sources (e.g. GitHub Issues
                            // poller) remove this task from their `dispatched` map. Without
                            // this the issue stays marked as dispatched forever and will never
                            // be re-queued, causing a silent production deadlock.
                            if let Some(cb) = &state.intake.completion_callback {
                                if let Some(final_state) = state.core.tasks.get(&task.id) {
                                    cb(final_state).await;
                                }
                            }
                            return;
                        }
                    };

                    // Issues 2 & 3: resolve canonical project path from repo name so that
                    // (a) the correct per-project concurrency bucket is used, and
                    // (b) req.project is populated so the agent runs in the right worktree.
                    let project_path = match task.repo.as_deref() {
                        Some(repo) => {
                            if let Some(registry) = state.core.project_registry.as_deref() {
                                match registry.resolve_path(repo).await {
                                    Ok(Some(p)) => Some(p),
                                    Ok(None) => Some(std::path::PathBuf::from(repo)),
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = ?task.id,
                                            repo,
                                            "startup recovery: registry lookup failed: {e}, using repo as path"
                                        );
                                        Some(std::path::PathBuf::from(repo))
                                    }
                                }
                            } else {
                                Some(std::path::PathBuf::from(repo))
                            }
                        }
                        None => None,
                    };

                    let canonical = match task_runner::resolve_canonical_project(project_path).await
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to resolve project path: {e}"
                            );
                            return;
                        }
                    };
                    let project_id = canonical.to_string_lossy().into_owned();

                    // Issue 1: acquire permit here inside the spawned future so serve()
                    // is never blocked waiting for a concurrency slot.
                    let permit = match state.concurrency.task_queue.acquire(&project_id).await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to acquire permit: {e}"
                            );
                            return;
                        }
                    };

                    let req = task_runner::CreateTaskRequest {
                        pr: Some(pr_num),
                        project: Some(canonical),
                        repo: task.repo.clone(),
                        source: task.source.clone(),
                        external_id: task.external_id.clone(),
                        ..Default::default()
                    };
                    let classification = crate::complexity_router::classify("", None, Some(pr_num));
                    let agent = match state.core.server.agent_registry.dispatch(&classification) {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::error!(
                                task_id = ?task.id,
                                "startup recovery: failed to dispatch agent: {e}"
                            );
                            return;
                        }
                    };
                    let (reviewer, _) = resolve_reviewer(
                        &state.core.server.agent_registry,
                        &state.core.server.config.agents.review,
                        agent.name(),
                    );
                    state.core.tasks.register_task_stream(&task.id);
                    task_runner::spawn_preregistered_task(
                        task.id,
                        state.core.tasks.clone(),
                        agent,
                        reviewer,
                        Arc::new(state.core.server.config.clone()),
                        state.engines.skills.clone(),
                        state.observability.events.clone(),
                        state.interceptors.clone(),
                        req,
                        state.concurrency.workspace_mgr.clone(),
                        permit,
                        state.intake.completion_callback.clone(),
                        None,
                    )
                    .await;
                });
            }
        }
    }

    let initial_grade = {
        let events = state
            .observability
            .events
            .query(&harness_core::types::EventFilters::default())
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
    // Pass the pre-built GitHub pollers from AppState to the orchestrator so
    // both share the same Arc instances and on_task_complete operates on the
    // live poller's dispatched map.
    let github_sources = state.intake.github_pollers.clone();
    crate::intake::build_orchestrator(
        &state.core.server.config.intake,
        Some(&expand_tilde(&state.core.server.config.server.data_dir)),
        state.intake.feishu_intake.clone(),
        github_sources,
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
        .route("/tasks/{id}/cancel", post(task_routes::cancel_task))
        .route("/tasks/{id}/artifacts", get(get_task_artifacts))
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
        .route("/api/dashboard", get(crate::handlers::dashboard::dashboard))
        .route("/api/intake", get(intake_status))
        .route(
            "/api/runtime-hosts",
            get(crate::handlers::runtime_hosts::list_runtime_hosts),
        )
        .route(
            "/api/runtime-hosts/register",
            post(crate::handlers::runtime_hosts::register_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/heartbeat",
            post(crate::handlers::runtime_hosts::heartbeat_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/deregister",
            post(crate::handlers::runtime_hosts::deregister_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/tasks/claim",
            post(crate::handlers::runtime_hosts::claim_task_for_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/projects",
            get(crate::handlers::runtime_project_cache::list_runtime_host_projects),
        )
        .route(
            "/api/runtime-hosts/{host_id}/projects/sync",
            post(crate::handlers::runtime_project_cache::sync_runtime_host_projects),
        )
        .route(
            "/api/token-usage",
            get(crate::handlers::token_usage::token_usage),
        )
        .route(
            "/webhook",
            post(github_webhook).layer(DefaultBodyLimit::max(
                state.core.server.config.server.max_webhook_body_bytes,
            )),
        )
        .route(
            "/webhook/feishu",
            post(crate::intake::feishu::feishu_webhook).layer(DefaultBodyLimit::max(
                state.core.server.config.server.max_webhook_body_bytes,
            )),
        )
        .route(
            "/signals",
            post(ingest_signal).layer(DefaultBodyLimit::max(
                state.core.server.config.server.max_webhook_body_bytes,
            )),
        )
        .route("/auth/reset-password", post(password_reset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
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
        Err(crate::services::execution::EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(crate::services::execution::EnqueueTaskError::Internal(error)) => (
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
    match state.core.tasks.get(&harness_core::types::TaskId(id)) {
        Some(task) => Json(task).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
    }
}

/// GET /tasks/{id}/artifacts — all persisted artifacts for a task.
async fn get_task_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    if state.core.tasks.get(&task_id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response();
    }
    match state.core.tasks.list_artifacts(&task_id).await {
        Ok(artifacts) => Json(artifacts).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
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
    let task_id = harness_core::types::TaskId(id);

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
                let data = match serde_json::to_string(&item) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("sse: failed to serialize event: {e}");
                        String::new()
                    }
                };
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

    // Send a heartbeat comment every 30 s so reverse proxies (nginx default
    // 60 s idle timeout) don't drop the connection while the agent is silent.
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(30))
                .text("heartbeat"),
        )
        .into_response()
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
            infer_github_severity(&req.payload).unwrap_or(harness_core::types::Severity::Low)
        } else {
            harness_core::types::Severity::Low
        }
    });

    let signal =
        harness_core::types::ExternalSignal::new(req.source.clone(), severity, req.payload.clone());
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

/// POST /auth/reset-password — initiate a password reset.
///
/// Rate-limited per email address to prevent enumeration and brute-force.
/// Always returns a generic success response regardless of whether the email
/// exists, to avoid leaking account information.
async fn password_reset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PasswordResetRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let email = req.email.trim().to_lowercase();
    if email.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "email is required"})),
        );
    }

    let limit = state
        .core
        .server
        .config
        .server
        .password_reset_rate_limit_per_hour;

    if !state
        .observability
        .password_reset_rate_limiter
        .check_and_increment(&email)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": format!(
                    "rate limit exceeded: max {} password reset requests per hour",
                    limit
                )
            })),
        );
    }

    tracing::info!(
        email_hash = %format!("{:x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            email.hash(&mut h);
            h.finish()
        }),
        "password reset requested"
    );

    (
        StatusCode::OK,
        Json(
            json!({"status": "ok", "message": "If that email is registered, a reset link has been sent."}),
        ),
    )
}

#[cfg(test)]
mod startup_tests {
    use super::build_app_state;
    use crate::{
        server::HarnessServer,
        test_helpers::{HomeGuard, HOME_LOCK},
        thread_manager::ThreadManager,
    };
    use harness_agents::registry::AgentRegistry;
    use harness_core::{
        config::HarnessConfig, types::EventFilters, types::RuleId, types::Severity,
        types::SkillLocation, types::Violation,
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn persisted_skills_survive_restart() -> anyhow::Result<()> {
        // Hold the shared HOME_LOCK so no sibling test races on HOME.
        let _lock = HOME_LOCK.lock().await;

        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let data_dir = sandbox.path().join("data");

        // Redirect HOME to an empty sandbox directory so that
        // $HOME/.harness/skills/ cannot shadow the persisted skill under
        // data_dir, keeping the test isolated from machine state.
        let fake_home = sandbox.path().join("home");
        std::fs::create_dir_all(&fake_home)?;
        // SAFETY: HOME_LOCK is held above; HomeGuard::drop restores HOME
        // unconditionally, even when an assertion below panics.
        let _env_guard = unsafe { HomeGuard::set(&fake_home) };

        let startup = |project_root: &std::path::Path, data_dir: &std::path::Path| {
            let project_root = project_root.to_path_buf();
            let data_dir = data_dir.to_path_buf();
            async move {
                let mut config = HarnessConfig::default();
                config.server.project_root = project_root;
                config.server.data_dir = data_dir;
                let server = Arc::new(HarnessServer::new(
                    config,
                    ThreadManager::new(),
                    AgentRegistry::new("test"),
                ));
                build_app_state(server).await
            }
        };

        // First startup: create a skill so it gets persisted to disk.
        {
            let state = startup(&project_root, &data_dir).await?;
            let mut skills = state.engines.skills.write().await;
            skills.create("my-test-skill".to_string(), "# My test skill".to_string());
        }

        // Assert the skill file was physically written to data_dir/skills/
        // before the second startup, catching a broken persist_dir path early.
        let persisted_path = data_dir.join("skills").join("my-test-skill.md");
        assert!(
            persisted_path.exists(),
            "expected skill file to be written to {}",
            persisted_path.display()
        );

        // Second startup: verify the persisted skill is reloaded via discover().
        {
            let state = startup(&project_root, &data_dir).await?;
            let skills = state.engines.skills.read().await;
            let reloaded = skills
                .list()
                .iter()
                .find(|s| s.name == "my-test-skill")
                .ok_or_else(|| {
                    anyhow::anyhow!("expected persisted skill to be reloaded after restart")
                })?;
            // Skills persisted via store.create() are stored in data_dir/skills/
            // and loaded with SkillLocation::User so they can override same-named
            // builtins after restart (User priority > System priority).
            assert_eq!(
                reloaded.location,
                SkillLocation::User,
                "reloaded skill has location {:?}; expected User (data_dir/skills/)",
                reloaded.location
            );
        }

        Ok(())
        // _env_guard dropped here → HOME restored unconditionally
        // _lock dropped here → next test may proceed
    }

    #[tokio::test]
    async fn build_app_state_auto_registers_builtin_guard() -> anyhow::Result<()> {
        let _lock = HOME_LOCK.lock().await;
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

    #[tokio::test]
    async fn startup_grade_uses_latest_rule_scan_session_for_violation_count() -> anyhow::Result<()>
    {
        let _lock = HOME_LOCK.lock().await;
        let sandbox = tempfile::tempdir()?;
        let project_root = sandbox.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let data_dir = sandbox.path().join("data");

        // Redirect HOME so build_app_state does not read from the real user home.
        let fake_home = sandbox.path().join("home");
        std::fs::create_dir_all(&fake_home)?;
        // SAFETY: HOME_LOCK is held above; HomeGuard::drop restores HOME unconditionally.
        let _env_guard = unsafe { HomeGuard::set(&fake_home) };

        let mut config = HarnessConfig::default();
        config.server.project_root = project_root.clone();
        config.server.data_dir = data_dir;
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let state = build_app_state(server).await?;

        // First scan: persist 5 violations (old session — must NOT count at startup).
        let old_violations: Vec<Violation> = (0..5)
            .map(|i| Violation {
                rule_id: RuleId::from_str(&format!("U-{i:02}")),
                file: std::path::PathBuf::from("src/old.rs"),
                line: Some(i + 1),
                message: format!("old violation {i}"),
                severity: Severity::Low,
            })
            .collect();
        state
            .observability
            .events
            .persist_rule_scan(&project_root, &old_violations)
            .await;

        // Second scan: persist 2 violations (latest session — must be used for startup grade).
        let new_violations = vec![
            Violation {
                rule_id: RuleId::from_str("SEC-01"),
                file: std::path::PathBuf::from("src/lib.rs"),
                line: Some(1),
                message: "new critical violation".to_string(),
                severity: Severity::Critical,
            },
            Violation {
                rule_id: RuleId::from_str("SEC-02"),
                file: std::path::PathBuf::from("src/main.rs"),
                line: None,
                message: "another new violation".to_string(),
                severity: Severity::High,
            },
        ];
        state
            .observability
            .events
            .persist_rule_scan(&project_root, &new_violations)
            .await;

        // Replicate the exact startup grade logic from serve() (lines 687-697).
        let events = state
            .observability
            .events
            .query(&EventFilters::default())
            .await
            .unwrap_or_default();
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

        // Must count only the latest scan session (2 violations), not historical total (7).
        assert_eq!(
            violation_count,
            new_violations.len(),
            "startup grade must use latest scan session ({} violations), not historical total ({})",
            new_violations.len(),
            old_violations.len() + new_violations.len(),
        );

        Ok(())
        // _env_guard dropped here → HOME restored unconditionally
        // _lock dropped here → next test may proceed
    }
}

#[cfg(test)]
mod tests;
