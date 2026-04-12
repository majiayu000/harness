use crate::{
    app_state::{
        AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
        NotificationServices, ObservabilityServices,
    },
    http::rate_limit,
    server::HarnessServer,
    task_runner,
};
use anyhow::Context;
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

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

/// Expand a leading `~/` or standalone `~` to the value of `$HOME`.
/// Returns the path unchanged when `~` is not present or `HOME` is unset.
pub(crate) fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                return std::path::PathBuf::from(home).join(rest);
            }
        } else if s == "~" {
            if let Ok(home) = std::env::var("HOME") {
                return std::path::PathBuf::from(home);
            }
        }
    }
    path.to_path_buf()
}

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

    let q_values_db_path = harness_core::config::dirs::default_db_path(&dir, "q_values");
    tracing::debug!("q_value db: {}", q_values_db_path.display());
    let q_values = match crate::q_value_store::QValueStore::open(&q_values_db_path).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!(
                path = %q_values_db_path.display(),
                "q_value store init failed, rule utility tracking will be disabled: {e}"
            );
            None
        }
    };

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
    // Terminal tasks are no longer held in the in-memory cache, so query DB directly.
    if let Some(ref wmgr) = workspace_mgr {
        match tasks.list_terminal_ids_from_db().await {
            Ok(terminal_ids) => {
                wmgr.cleanup_orphan_worktrees(&project_root, &terminal_ids)
                    .await;
            }
            Err(e) => {
                tracing::warn!("Failed to load terminal tasks for orphan worktree cleanup: {e}; skipping cleanup");
            }
        }
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
        // Select a read-only challenger dynamically: find the first registered agent
        // that (a) differs from the default primary and (b) does not advertise
        // Write or Execute capabilities.  Hard-coding "anthropic-api" makes the gate
        // unreachable when that agent is not registered or when it is also the
        // configured default (which triggers the identity guard in QualityTrigger).
        let default_name = server
            .agent_registry
            .resolved_default_agent_name()
            .map(|s| s.to_owned());
        let all_names: Vec<String> = server
            .agent_registry
            .list()
            .iter()
            .map(|&s| s.to_owned())
            .collect();
        let challenger = all_names.iter().find_map(|name| {
            if Some(name.as_str()) == default_name.as_deref() {
                return None;
            }
            let agent = server.agent_registry.get(name)?;
            if agent.capabilities().iter().any(|c| {
                matches!(
                    c,
                    harness_core::types::Capability::Write
                        | harness_core::types::Capability::Execute
                )
            }) {
                None
            } else {
                Some(agent)
            }
        });
        Arc::new(crate::quality_trigger::QualityTrigger::new(
            events.clone(),
            gc_agent.clone(),
            server.agent_registry.clone(),
            project_root.clone(),
            gc_cfg.auto_gc_grades.clone(),
            gc_cfg.auto_gc_cooldown_secs,
            challenger,
        ))
    };

    let completion_callback = build_completion_callback(
        &feishu_intake,
        &github_pollers,
        server.config.agents.review.clone(),
        Some(quality_trigger),
        server.config.server.github_token.clone(),
    );

    // Wrap completion callback to record Q-value pipeline events and apply backprop on
    // every live task completion (Done/Failed).  This ensures MemRL updates fire during
    // normal server operation, not only at startup recovery (validate_recovered_tasks).
    // Guard IDs are captured once at startup; they are stable after registration.
    let completion_callback = {
        if let Some(ref qv) = q_values {
            let qv = qv.clone();
            let inner = completion_callback;
            let cb: task_runner::CompletionCallback =
                std::sync::Arc::new(move |state: task_runner::TaskState| {
                    let qv = qv.clone();
                    let inner = inner.clone();
                    Box::pin(async move {
                        // Apply Q-value backprop only for terminal task states and only for
                        // experiences explicitly recorded via record_pipeline_event during
                        // task execution. Non-terminal states (Pending, Implementing, etc.)
                        // must not trigger Q-updates — they would incorrectly penalize rules
                        // that are still in the middle of a task.
                        let reward = match state.status {
                            task_runner::TaskStatus::Done => {
                                Some(crate::q_value_store::REWARD_MERGED)
                            }
                            task_runner::TaskStatus::Failed => {
                                Some(crate::q_value_store::REWARD_CLOSED)
                            }
                            task_runner::TaskStatus::Cancelled => {
                                Some(crate::q_value_store::REWARD_UNKNOWN_CLOSED)
                            }
                            _ => None,
                        };
                        if let Some(reward) = reward {
                            match qv.get_experiences_for_task(&state.id.0).await {
                                Ok(exp_ids) if !exp_ids.is_empty() => {
                                    if let Err(e) = qv
                                        .apply_q_update(
                                            &exp_ids,
                                            reward,
                                            crate::q_value_store::DEFAULT_ALPHA,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            task_id = %state.id.0,
                                            "q_value apply_q_update failed: {e}"
                                        );
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => tracing::warn!(
                                    task_id = %state.id.0,
                                    "q_value get_experiences_for_task failed: {e}"
                                ),
                            }
                        }
                        if let Some(cb) = inner {
                            cb(state).await;
                        }
                    })
                });
            Some(cb)
        } else {
            completion_callback
        }
    };

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
                let (ready_ids, failed_ids) = crate::task_runner::check_awaiting_deps(&store).await;
                for task_id in ready_ids.iter().chain(failed_ids.iter()) {
                    if let Err(e) = store.persist(task_id).await {
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
            q_values,
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
        runtime_state_persist_lock: Mutex::new(()),
        runtime_state_dirty: AtomicBool::new(false),
        notifications: NotificationServices {
            notification_tx: broadcast::channel(notification_broadcast_capacity).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(false)),
            initialized: Arc::new(AtomicBool::new(false)),
            ws_shutdown_tx: broadcast::channel(1).0,
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

fn build_completion_callback(
    feishu_intake: &Option<Arc<crate::intake::feishu::FeishuIntake>>,
    github_pollers: &[(String, Arc<dyn crate::intake::IntakeSource>)],
    review_config: harness_core::config::agents::AgentReviewConfig,
    quality_trigger: Option<Arc<crate::quality_trigger::QualityTrigger>>,
    config_github_token: Option<String>,
) -> Option<task_runner::CompletionCallback> {
    let mut sources: std::collections::HashMap<String, Arc<dyn crate::intake::IntakeSource>> =
        std::collections::HashMap::new();
    // Insert each GitHub poller keyed by "github:{owner/repo}" for precise
    // per-repo routing. Also insert the first poller under the bare "github"
    // key as a backward-compat fallback for tasks that pre-date multi-repo
    // support and have task.repo == None.
    for (i, (key, poller)) in github_pollers.iter().enumerate() {
        sources.insert(key.clone(), poller.clone());
        if i == 0 {
            sources.insert("github".to_string(), poller.clone());
        }
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
        let github_token = config_github_token.clone();
        Box::pin(async move {
            // Grade recent events and auto-trigger GC if quality is poor.
            if let Some(qt) = quality_trigger {
                let task_ctx = task.pr_url.as_ref().and_then(|pr| {
                    // Find the most recent implementation-affecting round.
                    // agent_review_fix rounds always store detail: None, so if
                    // the most recent such round is a fix round we have no
                    // usable diff for the final PR state — skip cross-review
                    // entirely to avoid judging stale initial-implement content
                    // and spuriously downgrading an already-fixed PR.
                    let last_impl_round = task
                        .rounds
                        .iter()
                        .rev()
                        .find(|r| r.action == "implement" || r.action == "agent_review_fix");
                    let diff = match last_impl_round {
                        Some(r) if r.action == "implement" => {
                            // Use the full implement-agent output as review context.
                            // The implementation prompt contract requires a PR_URL line
                            // but does not mandate unified-diff format, so accepting
                            // any non-empty detail avoids silently dropping valid rounds.
                            r.detail.clone().unwrap_or_default()
                        }
                        // agent_review_fix (detail always None) or no round at all
                        _ => String::new(),
                    };
                    if diff.is_empty() {
                        None
                    } else {
                        Some(crate::quality_trigger::TaskReviewContext {
                            diff,
                            pr_description: pr.clone(),
                        })
                    }
                });
                qt.check_and_maybe_trigger(task_ctx.as_ref()).await;
            }

            // Auto-trigger review bot comment when task completes with a PR URL.
            if review_config.review_bot_auto_trigger {
                if let task_runner::TaskStatus::Done = &task.status {
                    if let Some(pr_url) = task.pr_url.as_deref() {
                        if let Some((owner, repo, pr_num)) =
                            harness_core::prompts::parse_github_pr_url(pr_url)
                        {
                            let resolved_token = github_token
                                .or_else(|| std::env::var("GITHUB_TOKEN").ok())
                                .filter(|t| !t.is_empty());
                            match resolved_token {
                                Some(token) => {
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
            // For GitHub tasks, route to the specific repo's poller using
            // "github:{owner/repo}". Fall back to the bare "github" key for
            // tasks persisted before multi-repo support (task.repo == None).
            let lookup_key = if source_name == "github" {
                task.repo
                    .as_ref()
                    .map(|r| format!("github:{r}"))
                    .unwrap_or_else(|| "github".to_string())
            } else {
                source_name.to_string()
            };
            let Some(source) = sources.get(&lookup_key) else {
                tracing::warn!(
                    task_id = ?task.id,
                    source = source_name,
                    lookup_key,
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
