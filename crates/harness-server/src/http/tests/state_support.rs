use super::*;

pub(super) struct CapturingAgent {
    pub(super) prompts: Mutex<Vec<String>>,
}

impl CapturingAgent {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
        })
    }
}

pub(super) struct RuntimeStreamAgent {
    pub(super) prompts: Mutex<Vec<String>>,
    pub(super) models: Mutex<Vec<Option<String>>>,
    pub(super) reasoning_efforts: Mutex<Vec<Option<String>>>,
    pub(super) sandbox_modes: Mutex<Vec<Option<SandboxMode>>>,
    pub(super) approval_policies: Mutex<Vec<Option<String>>>,
}

pub(super) struct FailingStreamAgent {
    pub(super) prompts: Mutex<Vec<String>>,
    error: String,
}

impl RuntimeStreamAgent {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
            reasoning_efforts: Mutex::new(Vec::new()),
            sandbox_modes: Mutex::new(Vec::new()),
            approval_policies: Mutex::new(Vec::new()),
        })
    }
}

impl FailingStreamAgent {
    pub(super) fn new(error: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
            error: error.into(),
        })
    }
}

pub(super) struct BlockingAgent {
    release_permits: Semaphore,
}

impl BlockingAgent {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            release_permits: Semaphore::new(0),
        })
    }

    async fn wait_until_released(&self) {
        let permit = self
            .release_permits
            .acquire()
            .await
            .expect("blocking agent release semaphore should stay open");
        permit.forget();
    }
}

pub(super) fn empty_agent_response() -> AgentResponse {
    AgentResponse {
        output: String::new(),
        stderr: String::new(),
        items: vec![],
        token_usage: TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost_usd: 0.0,
        },
        model: "mock".into(),
        exit_code: Some(0),
    }
}

pub(super) fn successful_agent_response() -> AgentResponse {
    AgentResponse {
        output: "done".to_string(),
        ..empty_agent_response()
    }
}

#[async_trait]
impl CodeAgent for CapturingAgent {
    fn name(&self) -> &str {
        "capturing-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.prompts.lock().await.push(req.prompt);
        Ok(empty_agent_response())
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl CodeAgent for RuntimeStreamAgent {
    fn name(&self) -> &str {
        "runtime-stream-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.models.lock().await.push(req.model.clone());
        self.reasoning_efforts
            .lock()
            .await
            .push(req.reasoning_effort.clone());
        self.sandbox_modes.lock().await.push(req.sandbox_mode);
        self.approval_policies
            .lock()
            .await
            .push(req.approval_policy.clone());
        self.prompts.lock().await.push(req.prompt);
        Ok(successful_agent_response())
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.models.lock().await.push(req.model.clone());
        self.reasoning_efforts
            .lock()
            .await
            .push(req.reasoning_effort.clone());
        self.sandbox_modes.lock().await.push(req.sandbox_mode);
        self.approval_policies
            .lock()
            .await
            .push(req.approval_policy.clone());
        // Probe the runtime-job activity name from the prompt so the fenced
        // result block reports the correct `activity`. The activity is the
        // top `Activity:` line in the prompt packet header. Falling back to
        // "implement_issue" keeps the existing tests stable when no header
        // is found.
        let activity = req
            .prompt
            .lines()
            .find_map(|line| line.strip_prefix("Activity: ").map(str::trim))
            .unwrap_or("implement_issue")
            .to_string();
        self.prompts.lock().await.push(req.prompt);
        let fenced = format!(
            "runtime done\n\n```harness-activity-result\n{{\"activity\":\"{activity}\",\"status\":\"succeeded\",\"summary\":\"runtime done\"}}\n```"
        );
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning { content: fenced },
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

#[async_trait]
impl CodeAgent for FailingStreamAgent {
    fn name(&self) -> &str {
        "failing-stream-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.prompts.lock().await.push(req.prompt);
        Err(harness_core::error::HarnessError::AgentExecution(
            self.error.clone(),
        ))
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.prompts.lock().await.push(req.prompt);
        Err(harness_core::error::HarnessError::AgentExecution(
            self.error.clone(),
        ))
    }
}

#[async_trait]
impl CodeAgent for BlockingAgent {
    fn name(&self) -> &str {
        "blocking-agent"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.wait_until_released().await;
        Ok(successful_agent_response())
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.wait_until_released().await;
        let _ = tx
            .send(StreamItem::MessageDelta {
                text: "done".to_string(),
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

pub(super) async fn make_test_state_with(
    dir: &std::path::Path,
    config: harness_core::config::HarnessConfig,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    make_test_state_with_project_root(dir, dir, config, agent_registry).await
}

pub(super) async fn make_test_state_with_project_root(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    mut config: harness_core::config::HarnessConfig,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    let db_state_guard = crate::test_helpers::acquire_db_state_guard().await;
    let database_url = crate::test_helpers::test_database_url()?;
    config.server.database_url = Some(database_url.clone());
    let feishu_intake = config.intake.feishu.as_ref().and_then(|cfg| {
        (cfg.enabled && crate::intake::feishu::has_verification_token(cfg))
            .then(|| Arc::new(crate::intake::feishu::FeishuIntake::new(cfg.clone())))
    });
    let thread_manager = crate::thread_manager::ThreadManager::new();
    let server = Arc::new(crate::server::HarnessServer::new(
        config,
        thread_manager,
        agent_registry,
    ));
    let tasks = task_runner::TaskStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir, "tasks"),
        Some(&database_url),
    )
    .await?;
    let postgres_catalog = crate::postgres_catalog::PostgresCatalogMonitor::new(
        tasks.postgres_pool(),
        crate::postgres_catalog::PostgresCatalogThresholds::from_server(&server.config.server),
    )
    .await;
    let events = Arc::new(
        harness_observe::event_store::EventStore::new_with_database_url(dir, Some(&database_url))
            .await?,
    );
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
    let thread_db = crate::thread_db::ThreadDb::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir, "threads"),
        Some(&database_url),
    )
    .await?;
    let _project_svc_tmp = crate::project_registry::ProjectRegistry::open_with_database_url(
        &harness_core::config::dirs::default_db_path(dir, "projects"),
        Some(&database_url),
    )
    .await?;
    let project_svc = crate::services::project::DefaultProjectService::new(
        _project_svc_tmp,
        project_root.to_path_buf(),
    );
    let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
    let task_queue = Arc::new(crate::task_queue::TaskQueue::new(
        &server.config.concurrency,
    ));
    let mut review_queue_config = server.config.concurrency.clone();
    review_queue_config.max_concurrent_tasks = server.config.review.max_concurrent_tasks.max(1);
    let review_task_queue = Arc::new(crate::task_queue::TaskQueue::new(&review_queue_config));
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        task_queue.clone(),
        review_task_queue.clone(),
        None,
        None,
        None,
        None,
        vec![],
    );
    drop(db_state_guard);
    Ok(Arc::new(AppState {
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
            workflow_runtime_store: None,
            project_registry: None,
            runtime_state_store: None,
            maintenance_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
        engines: crate::http::EngineServices {
            skills: Arc::new(tokio::sync::RwLock::new(
                harness_skills::store::SkillStore::new(),
            )),
            rules: Arc::new(tokio::sync::RwLock::new(
                harness_rules::engine::RuleEngine::new(),
            )),
            gc_agent,
        },
        observability: crate::http::ObservabilityServices {
            alerts: crate::alerting::AlertHandle::disabled(),
            events,
            signal_rate_limiter: Arc::new(crate::http::rate_limit::SignalRateLimiter::new(100)),
            password_reset_rate_limiter: Arc::new(
                crate::http::rate_limit::PasswordResetRateLimiter::new(5),
            ),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue,
            review_task_queue,
            workspace_mgr: None,
        },
        #[cfg(test)]
        _db_state_guard: None,
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        postgres_catalog,
        isolation_availability: Default::default(),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
        runtime_circuit_breakers: Arc::new(
            crate::runtime_circuit_breaker::RuntimeCircuitBreakerRegistry::new(Default::default()),
        ),
        notifications: crate::http::NotificationServices {
            notification_tx: tokio::sync::broadcast::channel(32).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(true)),
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
        },
        interceptors: vec![],
        startup_statuses: vec![],
        degraded_subsystems: vec![],
        intake: crate::http::IntakeServices {
            feishu_intake,
            github_pollers: vec![],
            github_poller_repos: vec![],
            completion_callback: None,
            token_dispatch_counters: crate::http::IntakeServices::new_token_dispatch_counters(),
            intake_bindings: crate::intake::binding::IntakeBindingRegistry::new(),
        },
        project_svc,
        task_svc,
        execution_svc,
    }))
}

pub(super) async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.database_url = Some(crate::test_helpers::test_database_url()?);
    make_test_state_with(
        dir,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

pub(super) async fn make_test_state_with_issue_workflows(
    dir: &std::path::Path,
) -> anyhow::Result<Arc<AppState>> {
    let state = make_test_state(dir).await?;
    let workflow_store = harness_workflow::issue_lifecycle::IssueWorkflowStore::open(
        &harness_core::config::dirs::default_db_path(dir, "issue_workflows"),
    )
    .await?;
    Ok(Arc::new(AppState {
        core: crate::http::CoreServices {
            server: state.core.server.clone(),
            project_root: state.core.project_root.clone(),
            home_dir: state.core.home_dir.clone(),
            tasks: state.core.tasks.clone(),
            thread_db: None,
            plan_db: None,
            plan_cache: state.core.plan_cache.clone(),
            issue_workflow_store: Some(Arc::new(workflow_store)),
            project_workflow_store: None,
            workflow_runtime_store: None,
            project_registry: None,
            runtime_state_store: None,
            maintenance_active: state.core.maintenance_active.clone(),
        },
        engines: crate::http::EngineServices {
            skills: state.engines.skills.clone(),
            rules: state.engines.rules.clone(),
            gc_agent: state.engines.gc_agent.clone(),
        },
        observability: crate::http::ObservabilityServices {
            alerts: crate::alerting::AlertHandle::disabled(),
            events: state.observability.events.clone(),
            signal_rate_limiter: state.observability.signal_rate_limiter.clone(),
            password_reset_rate_limiter: state.observability.password_reset_rate_limiter.clone(),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: state.concurrency.task_queue.clone(),
            review_task_queue: state.concurrency.review_task_queue.clone(),
            workspace_mgr: None,
        },
        #[cfg(test)]
        _db_state_guard: None,
        runtime_hosts: state.runtime_hosts.clone(),
        runtime_project_cache: state.runtime_project_cache.clone(),
        postgres_catalog: state.postgres_catalog.clone(),
        isolation_availability: state.isolation_availability.clone(),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
        runtime_circuit_breakers: state.runtime_circuit_breakers.clone(),
        notifications: crate::http::NotificationServices {
            notification_tx: tokio::sync::broadcast::channel(32).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(true)),
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
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
        project_svc: state.project_svc.clone(),
        task_svc: state.task_svc.clone(),
        execution_svc: state.execution_svc.clone(),
    }))
}

pub(super) async fn make_test_state_with_workflow_runtime(
    dir: &std::path::Path,
) -> anyhow::Result<Arc<AppState>> {
    make_test_state_with_workflow_runtime_and_registry(
        dir,
        dir,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

pub(super) async fn make_test_state_with_workflow_runtime_and_registry(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    make_test_state_with_workflow_runtime_config_and_registry(
        dir,
        project_root,
        harness_core::config::HarnessConfig::default(),
        agent_registry,
    )
    .await
}

pub(super) async fn make_test_state_with_workflow_runtime_config_and_registry(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    config: harness_core::config::HarnessConfig,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    let state =
        make_test_state_with_project_root(dir, project_root, config, agent_registry).await?;
    let workflow_runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &harness_core::config::dirs::default_db_path(dir, "workflow_runtime"),
            Some(&crate::test_helpers::test_database_url()?),
        )
        .await?,
    );
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        state.core.tasks.clone(),
        state.core.server.agent_registry.clone(),
        Arc::new(state.core.server.config.clone()),
        state.engines.skills.clone(),
        state.observability.events.clone(),
        state.interceptors.clone(),
        None,
        state.concurrency.task_queue.clone(),
        state.concurrency.review_task_queue.clone(),
        None,
        None,
        Some(workflow_runtime_store.clone()),
        None,
        vec![],
    );
    Ok(Arc::new(AppState {
        core: crate::http::CoreServices {
            server: state.core.server.clone(),
            project_root: state.core.project_root.clone(),
            home_dir: state.core.home_dir.clone(),
            tasks: state.core.tasks.clone(),
            thread_db: state.core.thread_db.clone(),
            plan_db: None,
            plan_cache: state.core.plan_cache.clone(),
            issue_workflow_store: None,
            project_workflow_store: None,
            workflow_runtime_store: Some(workflow_runtime_store),
            project_registry: None,
            runtime_state_store: None,
            maintenance_active: state.core.maintenance_active.clone(),
        },
        engines: crate::http::EngineServices {
            skills: state.engines.skills.clone(),
            rules: state.engines.rules.clone(),
            gc_agent: state.engines.gc_agent.clone(),
        },
        observability: crate::http::ObservabilityServices {
            alerts: crate::alerting::AlertHandle::disabled(),
            events: state.observability.events.clone(),
            signal_rate_limiter: state.observability.signal_rate_limiter.clone(),
            password_reset_rate_limiter: state.observability.password_reset_rate_limiter.clone(),
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue: state.concurrency.task_queue.clone(),
            review_task_queue: state.concurrency.review_task_queue.clone(),
            workspace_mgr: None,
        },
        #[cfg(test)]
        _db_state_guard: None,
        runtime_hosts: state.runtime_hosts.clone(),
        runtime_project_cache: state.runtime_project_cache.clone(),
        postgres_catalog: state.postgres_catalog.clone(),
        isolation_availability: state.isolation_availability.clone(),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
        runtime_circuit_breakers: state.runtime_circuit_breakers.clone(),
        notifications: crate::http::NotificationServices {
            notification_tx: tokio::sync::broadcast::channel(32).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(true)),
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
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
        project_svc: state.project_svc.clone(),
        task_svc: state.task_svc.clone(),
        execution_svc,
    }))
}

#[tokio::test]
async fn persist_runtime_state_is_serialized() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let lock_guard = state.runtime_state_persist_lock.lock().await;

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let state_for_task = state.clone();
    let persist_task = tokio::spawn(async move {
        let _ = started_tx.send(());
        state_for_task.persist_runtime_state().await
    });

    started_rx.await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !persist_task.is_finished(),
        "persist_runtime_state should wait for the in-flight persist lock"
    );

    drop(lock_guard);
    tokio::time::timeout(Duration::from_secs(1), persist_task).await???;
    Ok(())
}

pub(super) async fn make_test_state_with_agent(
    dir: &std::path::Path,
    webhook_secret: Option<&str>,
) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = webhook_secret.map(ToString::to_string);
    build_test_state_with_agent(dir, dir, config).await
}

pub(super) async fn make_test_state_with_agent_and_config(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    config: harness_core::config::HarnessConfig,
) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
    build_test_state_with_agent(dir, project_root, config).await
}

pub(super) async fn build_test_state_with_agent(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    config: harness_core::config::HarnessConfig,
) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
    let capturing = CapturingAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("test");
    registry.register("test", capturing.clone());

    let state = make_test_state_with_project_root(dir, project_root, config, registry).await?;
    Ok((state, capturing))
}
