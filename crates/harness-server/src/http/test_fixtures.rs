use super::{
    AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
    NotificationServices, ObservabilityServices,
};
use crate::{
    project_registry::Project,
    server::HarnessServer,
    services::{
        execution::{EnqueueBackgroundOptions, EnqueueTaskError, ExecutionService, QueueDomain},
        project::ProjectService,
        task::DefaultTaskService,
    },
    task_runner::{CreateTaskRequest, TaskId, TaskStore},
    thread_manager::ThreadManager,
};
use async_trait::async_trait;
use harness_agents::registry::AgentRegistry;
use harness_core::config::{dirs::default_db_path, HarnessConfig};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc,
    },
};
use tokio::sync::RwLock;

struct ReadOnlyRouteProjectService {
    store: RwLock<HashMap<String, Project>>,
    root: PathBuf,
}

impl ReadOnlyRouteProjectService {
    fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            store: RwLock::new(HashMap::new()),
            root,
        })
    }
}

#[async_trait]
impl ProjectService for ReadOnlyRouteProjectService {
    async fn register(&self, project: Project) -> anyhow::Result<()> {
        self.store.write().await.insert(project.id.clone(), project);
        Ok(())
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
        Ok(self.store.read().await.get(id).cloned())
    }

    async fn get_by_name(&self, name: &str) -> anyhow::Result<Option<Project>> {
        Ok(self
            .store
            .read()
            .await
            .values()
            .find(|project| project.name.as_deref() == Some(name))
            .cloned())
    }

    async fn list(&self) -> anyhow::Result<Vec<Project>> {
        Ok(self.store.read().await.values().cloned().collect())
    }

    async fn remove(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.store.write().await.remove(id).is_some())
    }

    async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        Ok(self
            .store
            .read()
            .await
            .get(id)
            .map(|project| project.root.clone()))
    }

    fn default_root(&self) -> &Path {
        &self.root
    }
}

struct ReadOnlyRouteExecutionService;

impl ReadOnlyRouteExecutionService {
    fn rejected() -> EnqueueTaskError {
        EnqueueTaskError::Internal("read-only route test fixture cannot enqueue tasks".to_string())
    }
}

#[async_trait]
impl ExecutionService for ReadOnlyRouteExecutionService {
    async fn enqueue(&self, _req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        Err(Self::rejected())
    }

    async fn enqueue_in_domain(
        &self,
        _req: CreateTaskRequest,
        _queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError> {
        Err(Self::rejected())
    }

    async fn enqueue_background(
        &self,
        _req: CreateTaskRequest,
    ) -> Result<TaskId, EnqueueTaskError> {
        Err(Self::rejected())
    }

    async fn enqueue_background_with_options(
        &self,
        _req: CreateTaskRequest,
        _options: EnqueueBackgroundOptions,
    ) -> Result<TaskId, EnqueueTaskError> {
        Err(Self::rejected())
    }
}

pub(super) async fn make_read_only_route_test_state(dir: &Path) -> anyhow::Result<Arc<AppState>> {
    make_read_only_route_test_state_with(dir, HarnessConfig::default(), AgentRegistry::new("test"))
        .await
}

pub(super) async fn make_read_only_route_test_state_with(
    dir: &Path,
    config: HarnessConfig,
    agent_registry: AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    make_read_only_route_test_state_with_project_root(dir, dir, config, agent_registry).await
}

async fn make_read_only_route_test_state_with_project_root(
    dir: &Path,
    project_root: &Path,
    mut config: HarnessConfig,
    agent_registry: AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    let db_state_guard = crate::test_helpers::acquire_db_state_guard().await;
    let database_url = crate::test_helpers::test_database_url()?;
    config.server.database_url = Some(database_url.clone());
    let feishu_intake = config.intake.feishu.as_ref().and_then(|cfg| {
        (cfg.enabled && crate::intake::feishu::has_verification_token(cfg))
            .then(|| Arc::new(crate::intake::feishu::FeishuIntake::new(cfg.clone())))
    });
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        agent_registry,
    ));
    let tasks = TaskStore::open_with_database_url(
        &default_db_path(dir, "tasks"),
        Some(database_url.as_str()),
    )
    .await?;
    drop(db_state_guard);

    let password_reset_rate_limit = server.config.server.password_reset_rate_limit_per_hour;
    let task_queue = Arc::new(crate::task_queue::TaskQueue::new(
        &server.config.concurrency,
    ));
    let mut review_queue_config = server.config.concurrency.clone();
    review_queue_config.max_concurrent_tasks = server.config.review.max_concurrent_tasks.max(1);
    let review_task_queue = Arc::new(crate::task_queue::TaskQueue::new(&review_queue_config));

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
    let (notification_tx, _) = tokio::sync::broadcast::channel(32);
    let (ws_shutdown_tx, _) = tokio::sync::broadcast::channel(1);
    let events = Arc::new(harness_observe::event_store::EventStore::new_noop_for_tests());
    let project_svc = ReadOnlyRouteProjectService::new(project_root.to_path_buf());
    let task_svc = DefaultTaskService::new(tasks.clone());
    let execution_svc: Arc<dyn ExecutionService> = Arc::new(ReadOnlyRouteExecutionService);

    Ok(Arc::new(AppState {
        core: CoreServices {
            server,
            project_root: project_root.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| project_root.to_path_buf()),
            tasks,
            thread_db: None,
            plan_db: None,
            plan_cache: Arc::new(dashmap::DashMap::new()),
            issue_workflow_store: None,
            project_workflow_store: None,
            workflow_runtime_store: None,
            project_registry: None,
            runtime_state_store: None,
            maintenance_active: Arc::new(AtomicBool::new(false)),
        },
        engines: EngineServices {
            skills: Arc::new(RwLock::new(harness_skills::store::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            gc_agent,
        },
        observability: ObservabilityServices {
            events,
            signal_rate_limiter: Arc::new(crate::http::rate_limit::SignalRateLimiter::new(100)),
            password_reset_rate_limiter: Arc::new(
                crate::http::rate_limit::PasswordResetRateLimiter::new(password_reset_rate_limit),
            ),
            review_store: None,
        },
        concurrency: ConcurrencyServices {
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
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: AtomicBool::new(false),
        runtime_circuit_breakers: Arc::new(
            crate::runtime_circuit_breaker::RuntimeCircuitBreakerRegistry::new(Default::default()),
        ),
        notifications: NotificationServices {
            notification_tx,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initializing: Arc::new(AtomicBool::new(true)),
            initialized: Arc::new(AtomicBool::new(true)),
            ws_shutdown_tx,
        },
        intake: IntakeServices {
            feishu_intake,
            github_pollers: vec![],
            github_poller_repos: vec![],
            completion_callback: None,
            token_dispatch_counters: IntakeServices::new_token_dispatch_counters(),
        },
        interceptors: vec![],
        startup_statuses: vec![],
        degraded_subsystems: vec![],
        project_svc,
        task_svc,
        execution_svc,
    }))
}
