use crate::{http, server::HarnessServer, task_runner};
use anyhow::Context;
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug)]
pub(crate) struct StartupBootstrap {
    pub(crate) data_dir: PathBuf,
    pub(crate) project_root: PathBuf,
    pub(crate) notification_broadcast_capacity: usize,
    pub(crate) notification_lag_log_every: u64,
    pub(crate) signal_rate_limit: u32,
    pub(crate) password_reset_rate_limit: u32,
    pub(crate) home_dir: PathBuf,
}

pub(crate) struct PersistenceStores {
    pub(crate) tasks: Arc<task_runner::TaskStore>,
    pub(crate) q_values: Option<Arc<crate::q_value_store::QValueStore>>,
    pub(crate) thread_db: crate::thread_db::ThreadDb,
    pub(crate) plan_db: crate::plan_db::PlanDb,
    pub(crate) project_registry: Arc<crate::project_registry::ProjectRegistry>,
}

pub(crate) struct EngineStartup {
    pub(crate) rules: Arc<RwLock<harness_rules::engine::RuleEngine>>,
    pub(crate) skills: Arc<RwLock<harness_skills::store::SkillStore>>,
}

pub(crate) struct ObservabilityStartup {
    pub(crate) events: Arc<harness_observe::event_store::EventStore>,
    pub(crate) gc_agent: Arc<harness_gc::gc_agent::GcAgent>,
    pub(crate) review_store: Option<Arc<crate::review_store::ReviewStore>>,
}

pub(crate) struct HydratedCaches {
    pub(crate) plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
}

pub(crate) struct ConcurrencyStartup {
    pub(crate) task_queue: Arc<crate::task_queue::TaskQueue>,
    pub(crate) workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
}

pub(crate) struct IntakeStartup {
    pub(crate) feishu_intake: Option<Arc<crate::intake::feishu::FeishuIntake>>,
    pub(crate) github_pollers: Vec<(String, Arc<dyn crate::intake::IntakeSource>)>,
    pub(crate) completion_callback: Option<task_runner::CompletionCallback>,
    pub(crate) interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    pub(crate) project_svc: Arc<dyn crate::services::project::ProjectService>,
    pub(crate) task_svc: Arc<dyn crate::services::task::TaskService>,
    pub(crate) execution_svc: Arc<dyn crate::services::execution::ExecutionService>,
}

pub(crate) struct RuntimeStartup {
    pub(crate) runtime_hosts: Arc<crate::runtime_hosts::RuntimeHostManager>,
    pub(crate) runtime_project_cache: Arc<crate::runtime_project_cache::RuntimeProjectCacheManager>,
    pub(crate) runtime_state_store: Option<Arc<crate::runtime_state_store::RuntimeStateStore>>,
}

pub(crate) fn bootstrap(server: &Arc<HarnessServer>) -> anyhow::Result<StartupBootstrap> {
    let data_dir = expand_tilde(&server.config.server.data_dir);
    let project_root = resolve_project_root(&server.config.server.project_root)?;
    std::fs::create_dir_all(&data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::symlink_metadata(&data_dir)
            .with_context(|| format!("failed to stat data_dir {:?}", data_dir))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "data_dir {:?} is a symbolic link; refusing to start to prevent potential symlink hijacking. Set an explicit `data_dir` in your harness config file.",
                data_dir
            );
        }
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("failed to set 0o700 permissions on data_dir {:?}", data_dir),
        )?;
    }

    tracing::debug!(
        data_dir = %data_dir.display(),
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

    let configured_capacity = server.config.server.notification_broadcast_capacity;
    let notification_broadcast_capacity = configured_capacity.max(1);
    if configured_capacity == 0 {
        tracing::warn!(
            "server.notification_broadcast_capacity=0 is invalid; falling back to capacity=1"
        );
    }

    let home_dir = system_home_dir().unwrap_or_else(|| project_root.clone());

    Ok(StartupBootstrap {
        data_dir,
        project_root,
        notification_broadcast_capacity,
        notification_lag_log_every: server.config.server.notification_lag_log_every,
        signal_rate_limit: server.config.server.signal_rate_limit_per_minute,
        password_reset_rate_limit: server.config.server.password_reset_rate_limit_per_hour,
        home_dir,
    })
}

pub(crate) async fn init_persistence(
    bootstrap: &StartupBootstrap,
) -> anyhow::Result<PersistenceStores> {
    let db_path = harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "tasks");
    tracing::debug!("task db: {}", db_path.display());
    let tasks = task_runner::TaskStore::open(&db_path).await?;

    let q_values_db_path =
        harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "q_values");
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

    let thread_db_path =
        harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "threads");
    let thread_db = crate::thread_db::ThreadDb::open(&thread_db_path).await?;
    let plan_db = crate::plan_db::PlanDb::open(&harness_core::config::dirs::default_db_path(
        &bootstrap.data_dir,
        "plans",
    ))
    .await?;
    let project_registry = crate::project_registry::ProjectRegistry::open(
        &harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "projects"),
    )
    .await?;

    Ok(PersistenceStores {
        tasks,
        q_values,
        thread_db,
        plan_db,
        project_registry,
    })
}

pub(crate) fn init_rules_and_skills(
    server: &Arc<HarnessServer>,
    bootstrap: &StartupBootstrap,
) -> anyhow::Result<EngineStartup> {
    let mut rule_engine = harness_rules::engine::RuleEngine::new();
    rule_engine.configure_sources(
        server.config.rules.discovery_paths.clone(),
        server.config.rules.builtin_path.clone(),
        server.config.rules.requirements_path.clone(),
    );
    if let Err(e) = rule_engine.load_builtin() {
        tracing::warn!("failed to load builtin rules: {e}");
    }
    match rule_engine.auto_register_builtin_guards(&bootstrap.data_dir) {
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
    match rule_engine.auto_register_project_guards(&bootstrap.project_root.join(".harness/guards"))
    {
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

    let mut skill_store = harness_skills::store::SkillStore::new()
        .with_persist_dir(bootstrap.data_dir.join("skills"))
        .with_discovery(&bootstrap.project_root);
    skill_store.load_builtin();
    if let Err(e) = skill_store.discover() {
        tracing::warn!("Failed to reload persisted skills on startup: {}", e);
    }

    Ok(EngineStartup {
        rules: Arc::new(RwLock::new(rule_engine)),
        skills: Arc::new(RwLock::new(skill_store)),
    })
}

pub(crate) async fn init_observability(
    server: &Arc<HarnessServer>,
    bootstrap: &StartupBootstrap,
) -> anyhow::Result<ObservabilityStartup> {
    let events = Arc::new(
        harness_observe::event_store::EventStore::with_policies_and_otel(
            &bootstrap.data_dir,
            server.config.observe.session_renewal_secs,
            server.config.observe.log_retention_days,
            &server.config.otel,
        )
        .await?,
    );

    let signal_detector = harness_gc::signal_detector::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::types::ProjectId::from_str(
            bootstrap
                .project_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("default"),
        ),
    );
    let draft_store = harness_gc::draft_store::DraftStore::new(&bootstrap.data_dir)?;
    let gc_agent = Arc::new(
        harness_gc::gc_agent::GcAgent::new(
            server.config.gc.clone(),
            signal_detector,
            draft_store,
            bootstrap.project_root.clone(),
        )
        .with_checkpoint(bootstrap.data_dir.join("gc-checkpoint.json")),
    );

    let review_db_path =
        harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "reviews");
    let review_store = match crate::review_store::ReviewStore::open(&review_db_path).await {
        Ok(store) => Some(Arc::new(store)),
        Err(e) => {
            tracing::warn!("review store init failed, reviews will not be persisted: {e}");
            None
        }
    };

    Ok(ObservabilityStartup {
        events,
        gc_agent,
        review_store,
    })
}

pub(crate) async fn hydrate_caches_and_projects(
    server: &Arc<HarnessServer>,
    bootstrap: &StartupBootstrap,
    stores: &PersistenceStores,
) -> anyhow::Result<HydratedCaches> {
    let default_project = crate::project_registry::Project {
        id: "default".to_string(),
        root: bootstrap.project_root.clone(),
        max_concurrent: None,
        default_agent: None,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = stores.project_registry.register(default_project).await {
        tracing::warn!("failed to auto-register default project: {e}");
    }
    for (name, path) in &server.startup_projects {
        let proj = crate::project_registry::Project {
            id: name.clone(),
            root: path.clone(),
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = stores.project_registry.register(proj).await {
            tracing::warn!(project = %name, "failed to register startup project: {e}");
        }
    }

    let plans_md_dir = bootstrap.data_dir.join("plans");
    match stores
        .plan_db
        .migrate_from_markdown_dir(&plans_md_dir)
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::debug!(
            count = n,
            "plan migration: imported {} plan(s) from markdown",
            n
        ),
        Err(e) => tracing::warn!("plan migration: failed: {e}"),
    }

    for thread in stores.thread_db.list().await? {
        server
            .thread_manager
            .threads_cache()
            .insert(thread.id.as_str().to_string(), thread);
    }

    let plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>> = Arc::new(DashMap::new());
    match stores.plan_db.list().await {
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

    Ok(HydratedCaches { plan_cache })
}

pub(crate) async fn init_concurrency(
    server: &Arc<HarnessServer>,
    bootstrap: &StartupBootstrap,
    tasks: &Arc<task_runner::TaskStore>,
) -> anyhow::Result<ConcurrencyStartup> {
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

    if let Some(ref wmgr) = workspace_mgr {
        match tasks.list_terminal_ids_from_db().await {
            Ok(terminal_ids) => {
                wmgr.cleanup_orphan_worktrees(&bootstrap.project_root, &terminal_ids)
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

    Ok(ConcurrencyStartup {
        task_queue,
        workspace_mgr,
    })
}

pub(crate) async fn init_intake_and_services(
    server: &Arc<HarnessServer>,
    bootstrap: &StartupBootstrap,
    stores: &PersistenceStores,
    engines: &EngineStartup,
    observability: &ObservabilityStartup,
    concurrency: &ConcurrencyStartup,
) -> anyhow::Result<IntakeStartup> {
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
                        Some(&bootstrap.data_dir),
                    )) as Arc<dyn crate::intake::IntakeSource>;
                    (key, poller)
                })
                .collect()
        })
        .unwrap_or_default();

    let quality_trigger = {
        let gc_cfg = &server.config.gc;
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
            observability.events.clone(),
            observability.gc_agent.clone(),
            server.agent_registry.clone(),
            bootstrap.project_root.clone(),
            gc_cfg.auto_gc_grades.clone(),
            gc_cfg.auto_gc_cooldown_secs,
            challenger,
        ))
    };

    let completion_callback = http::build_completion_callback(
        &feishu_intake,
        &github_pollers,
        server.config.agents.review.clone(),
        Some(quality_trigger),
        server.config.server.github_token.clone(),
    );

    let completion_callback = if let Some(ref qv) = stores.q_values {
        let qv = qv.clone();
        let inner = completion_callback;
        let cb: task_runner::CompletionCallback =
            std::sync::Arc::new(move |state: task_runner::TaskState| {
                let qv = qv.clone();
                let inner = inner.clone();
                Box::pin(async move {
                    let reward = match state.status {
                        task_runner::TaskStatus::Done => {
                            if state.pr_url.is_some() {
                                Some(crate::q_value_store::REWARD_MERGED)
                            } else {
                                None
                            }
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
    };

    let hook_enforcement = server.config.rules.hook_enforcement;
    let events_for_hooks = observability.events.clone();
    let validation_config = server.config.validation.clone();
    let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> = vec![
        Arc::new(crate::contract_validator::ContractValidator::new()),
        Arc::new(crate::rule_enforcer::RuleEnforcer::new(
            engines.rules.clone(),
        )),
        Arc::new(crate::hook_enforcer::HookEnforcer::new(
            engines.rules.clone(),
            events_for_hooks,
            hook_enforcement,
        )),
        Arc::new(crate::post_validator::PostExecutionValidator::new(
            validation_config,
        )),
    ];

    let project_svc = crate::services::project::DefaultProjectService::new(
        stores.project_registry.clone(),
        bootstrap.project_root.clone(),
    );
    let task_svc = crate::services::task::DefaultTaskService::new(stores.tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        stores.tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        engines.skills.clone(),
        observability.events.clone(),
        interceptors.clone(),
        concurrency.workspace_mgr.clone(),
        concurrency.task_queue.clone(),
        completion_callback.clone(),
        Some(stores.project_registry.clone()),
        server.config.server.allowed_project_roots.clone(),
    );

    Ok(IntakeStartup {
        feishu_intake,
        github_pollers,
        completion_callback,
        interceptors,
        project_svc,
        task_svc,
        execution_svc,
    })
}

pub(crate) fn spawn_background_tasks(
    tasks: &Arc<task_runner::TaskStore>,
    completion_callback: Option<task_runner::CompletionCallback>,
) {
    {
        let tasks_for_recovery = tasks.clone();
        let cb_for_recovery = completion_callback.clone();
        tokio::spawn(async move {
            tasks_for_recovery
                .validate_recovered_tasks(cb_for_recovery)
                .await;
        });
    }

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
}

pub(crate) async fn restore_runtime_state(
    bootstrap: &StartupBootstrap,
) -> anyhow::Result<RuntimeStartup> {
    let runtime_hosts = Arc::new(crate::runtime_hosts::RuntimeHostManager::new());
    let runtime_project_cache =
        Arc::new(crate::runtime_project_cache::RuntimeProjectCacheManager::new());
    let runtime_state_db_path =
        harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "runtime_state");
    let runtime_state_store =
        match crate::runtime_state_store::RuntimeStateStore::open(&runtime_state_db_path).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(
                    path = %runtime_state_db_path.display(),
                    "runtime state store init failed, runtime host state will not persist: {e}"
                );
                None
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

    Ok(RuntimeStartup {
        runtime_hosts,
        runtime_project_cache,
        runtime_state_store,
    })
}

pub(crate) fn assemble_app_state(
    server: Arc<HarnessServer>,
    bootstrap: StartupBootstrap,
    stores: PersistenceStores,
    engines: EngineStartup,
    observability: ObservabilityStartup,
    cache: HydratedCaches,
    concurrency: ConcurrencyStartup,
    intake: IntakeStartup,
    runtime: RuntimeStartup,
) -> http::AppState {
    http::AppState {
        core: http::CoreServices {
            server,
            project_root: bootstrap.project_root,
            home_dir: bootstrap.home_dir,
            tasks: stores.tasks,
            thread_db: Some(stores.thread_db),
            plan_db: Some(stores.plan_db),
            plan_cache: cache.plan_cache,
            project_registry: Some(stores.project_registry),
            runtime_state_store: runtime.runtime_state_store,
            q_values: stores.q_values,
        },
        engines: http::EngineServices {
            skills: engines.skills,
            rules: engines.rules,
            gc_agent: observability.gc_agent,
        },
        observability: http::ObservabilityServices {
            events: observability.events,
            signal_rate_limiter: Arc::new(http::rate_limit::SignalRateLimiter::new(
                bootstrap.signal_rate_limit,
            )),
            password_reset_rate_limiter: Arc::new(http::rate_limit::PasswordResetRateLimiter::new(
                bootstrap.password_reset_rate_limit,
            )),
            review_store: observability.review_store,
        },
        concurrency: http::ConcurrencyServices {
            task_queue: concurrency.task_queue,
            workspace_mgr: concurrency.workspace_mgr,
        },
        runtime_hosts: runtime.runtime_hosts,
        runtime_project_cache: runtime.runtime_project_cache,
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: AtomicBool::new(false),
        notifications: http::NotificationServices {
            notification_tx: tokio::sync::broadcast::channel(
                bootstrap.notification_broadcast_capacity,
            )
            .0,
            notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notification_lag_log_every: bootstrap.notification_lag_log_every,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(false)),
            initialized: Arc::new(AtomicBool::new(false)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        intake: http::IntakeServices {
            feishu_intake: intake.feishu_intake,
            github_pollers: intake.github_pollers.into_iter().map(|(_, p)| p).collect(),
            completion_callback: intake.completion_callback,
        },
        interceptors: intake.interceptors,
        project_svc: intake.project_svc,
        task_svc: intake.task_svc,
        execution_svc: intake.execution_svc,
    }
}

fn resolve_project_root(configured_root: &Path) -> anyhow::Result<PathBuf> {
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

fn system_home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(rest) = s.strip_prefix("~/") {
            if let Some(home) = system_home_dir() {
                return home.join(rest);
            }
        } else if s == "~" {
            if let Some(home) = system_home_dir() {
                return home;
            }
        }
    }
    path.to_path_buf()
}
