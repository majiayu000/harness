use super::*;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use chrono::Utc;
use harness_core::config::agents::SandboxMode;
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, agent::StreamItem,
    types::Capability, types::Item, types::TokenUsage, types::TurnFailure, types::TurnFailureKind,
    types::TurnTelemetry,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tower::ServiceExt;

struct CapturingAgent {
    prompts: Mutex<Vec<String>>,
}

impl CapturingAgent {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
        })
    }
}

struct RuntimeStreamAgent {
    prompts: Mutex<Vec<String>>,
    models: Mutex<Vec<Option<String>>>,
    reasoning_efforts: Mutex<Vec<Option<String>>>,
    sandbox_modes: Mutex<Vec<Option<SandboxMode>>>,
    approval_policies: Mutex<Vec<Option<String>>>,
}

impl RuntimeStreamAgent {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
            reasoning_efforts: Mutex::new(Vec::new()),
            sandbox_modes: Mutex::new(Vec::new()),
            approval_policies: Mutex::new(Vec::new()),
        })
    }
}

struct BlockingAgent {
    release_permits: Semaphore,
}

impl BlockingAgent {
    fn new() -> Arc<Self> {
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

fn empty_agent_response() -> AgentResponse {
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

fn successful_agent_response() -> AgentResponse {
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
        self.prompts.lock().await.push(req.prompt);
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: "runtime done".to_string(),
                },
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
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

async fn make_test_state_with(
    dir: &std::path::Path,
    config: harness_core::config::HarnessConfig,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    make_test_state_with_project_root(dir, dir, config, agent_registry).await
}

async fn make_test_state_with_project_root(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    mut config: harness_core::config::HarnessConfig,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
    let _ = crate::test_helpers::test_database_url()?;
    let db_state_guard = crate::test_helpers::acquire_db_state_guard().await;
    let database_url = crate::test_helpers::ensure_test_database_url_override()?;
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
            q_values: None,
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
            events,
            signal_rate_limiter: Arc::new(crate::http::rate_limit::SignalRateLimiter::new(100)),
            password_reset_rate_limiter: Arc::new(
                crate::http::rate_limit::PasswordResetRateLimiter::new(5),
            ),
            review_store: None,
        },
        concurrency: crate::http::ConcurrencyServices {
            task_queue,
            review_task_queue,
            workspace_mgr: None,
        },
        #[cfg(test)]
        _db_state_guard: Some(db_state_guard),
        runtime_hosts: Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
        runtime_project_cache: Arc::new(
            crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
        ),
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
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
            completion_callback: None,
        },
        project_svc,
        task_svc,
        execution_svc,
    }))
}

async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.database_url = Some(crate::test_helpers::test_database_url()?);
    make_test_state_with(
        dir,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

async fn make_test_state_with_issue_workflows(
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
            q_values: None,
            maintenance_active: state.core.maintenance_active.clone(),
        },
        engines: crate::http::EngineServices {
            skills: state.engines.skills.clone(),
            rules: state.engines.rules.clone(),
            gc_agent: state.engines.gc_agent.clone(),
        },
        observability: crate::http::ObservabilityServices {
            events: state.observability.events.clone(),
            signal_rate_limiter: state.observability.signal_rate_limiter.clone(),
            password_reset_rate_limiter: state.observability.password_reset_rate_limiter.clone(),
            review_store: None,
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
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
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
            completion_callback: None,
        },
        project_svc: state.project_svc.clone(),
        task_svc: state.task_svc.clone(),
        execution_svc: state.execution_svc.clone(),
    }))
}

async fn make_test_state_with_workflow_runtime(
    dir: &std::path::Path,
) -> anyhow::Result<Arc<AppState>> {
    make_test_state_with_workflow_runtime_and_registry(
        dir,
        dir,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

async fn make_test_state_with_workflow_runtime_and_registry(
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

async fn make_test_state_with_workflow_runtime_config_and_registry(
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
            q_values: None,
            maintenance_active: state.core.maintenance_active.clone(),
        },
        engines: crate::http::EngineServices {
            skills: state.engines.skills.clone(),
            rules: state.engines.rules.clone(),
            gc_agent: state.engines.gc_agent.clone(),
        },
        observability: crate::http::ObservabilityServices {
            events: state.observability.events.clone(),
            signal_rate_limiter: state.observability.signal_rate_limiter.clone(),
            password_reset_rate_limiter: state.observability.password_reset_rate_limiter.clone(),
            review_store: None,
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
        runtime_state_persist_lock: tokio::sync::Mutex::new(()),
        runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
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
            completion_callback: None,
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

async fn make_test_state_with_agent(
    dir: &std::path::Path,
    webhook_secret: Option<&str>,
) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = webhook_secret.map(ToString::to_string);
    build_test_state_with_agent(dir, dir, config).await
}

async fn make_test_state_with_agent_and_config(
    dir: &std::path::Path,
    project_root: &std::path::Path,
    config: harness_core::config::HarnessConfig,
) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
    build_test_state_with_agent(dir, project_root, config).await
}

async fn build_test_state_with_agent(
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

fn init_fake_git_repo(root: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join(".git"))?;
    Ok(())
}

fn task_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/tasks", post(task_routes::create_task))
        .route("/tasks/batch", post(task_routes::create_tasks_batch))
        .route("/tasks/{id}", get(get_task))
        .with_state(state)
}

fn intake_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/intake", get(intake_status))
        .with_state(state)
}

fn token_usage_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/token-usage",
            get(crate::handlers::token_usage::token_usage),
        )
        .with_state(state)
}

fn workflow_runtime_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/workflows/runtime/tree",
            get(get_workflow_runtime_tree),
        )
        .with_state(state)
}

fn webhook_app(state: Arc<AppState>) -> Router {
    let body_limit = state.core.server.config.server.max_webhook_body_bytes;
    Router::new()
        .route(
            "/webhook",
            post(github_webhook).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/webhook/feishu",
            post(crate::intake::feishu::feishu_webhook).layer(DefaultBodyLimit::max(body_limit)),
        )
        .with_state(state)
}

fn make_feishu_config(
    verification_token: Option<&str>,
) -> harness_core::config::intake::FeishuIntakeConfig {
    harness_core::config::intake::FeishuIntakeConfig {
        enabled: true,
        app_id: None,
        app_secret: None,
        verification_token: verification_token.map(ToString::to_string),
        trigger_keyword: "harness".to_string(),
        default_repo: None,
    }
}

async fn make_test_state_with_feishu(
    dir: &std::path::Path,
    verification_token: Option<&str>,
) -> anyhow::Result<Arc<AppState>> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.feishu = Some(make_feishu_config(verification_token));
    make_test_state_with(
        dir,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

fn feishu_challenge_payload(token: Option<&str>) -> serde_json::Value {
    match token {
        Some(token) => serde_json::json!({ "challenge": "challenge-123", "token": token }),
        None => serde_json::json!({ "challenge": "challenge-123" }),
    }
}

fn feishu_event_payload(token: Option<&str>) -> serde_json::Value {
    let content = serde_json::to_string(&serde_json::json!({ "text": "harness fix login bug" }))
        .expect("serialize feishu content");
    match token {
        Some(token) => serde_json::json!({
            "header": { "event_type": "im.message.receive_v1", "token": token },
            "event": {
                "message": {
                    "message_id": "msg-001",
                    "chat_id": "chat-001",
                    "message_type": "text",
                    "content": content
                }
            }
        }),
        None => serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "message": {
                    "message_id": "msg-001",
                    "chat_id": "chat-001",
                    "message_type": "text",
                    "content": content
                }
            }
        }),
    }
}

async fn response_json(response: axum::response::Response) -> anyhow::Result<serde_json::Value> {
    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

async fn assert_runtime_issue_submission(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    repo: Option<&str>,
    issue_number: u64,
    task_id: &str,
) -> anyhow::Result<String> {
    let task_id = task_runner::TaskId::from_str(task_id);
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none(),
        "workflow runtime issue submissions must not register legacy task rows"
    );
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        repo,
        issue_number,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));

    let get_response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/tasks/{}", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_response.status(), StatusCode::OK);
    let runtime_task = response_json(get_response).await?;
    assert_eq!(runtime_task["task_id"], task_id.0);
    assert_eq!(runtime_task["status"], "implementing");
    assert_eq!(runtime_task["execution_path"], "workflow_runtime");
    assert_eq!(runtime_task["workflow_id"], workflow_id);
    Ok(workflow_id)
}

async fn seed_bound_runtime_pr_workflow(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    repo: &str,
    issue_number: u64,
    pr_number: u64,
) -> anyhow::Result<(String, String)> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let project_id = canonical_project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some(repo), issue_number);
    let task_id = format!("runtime-issue-{issue_number}");
    let instance = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id,
        "repo": repo,
        "issue_number": issue_number,
        "task_id": task_id.clone(),
        "task_ids": [task_id.clone()],
        "pr_number": pr_number,
        "pr_url": format!("https://github.com/{repo}/pull/{pr_number}"),
        "execution_path": "workflow_runtime"
    }));
    store.upsert_instance(&instance).await?;
    Ok((workflow_id, task_id))
}

async fn assert_runtime_pr_feedback_requested(
    state: &Arc<AppState>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let instance = store
        .get_instance(workflow_id)
        .await?
        .expect("runtime workflow should exist");
    assert_eq!(instance.state, "awaiting_feedback");
    assert_eq!(instance.data["task_id"], task_id);
    let commands = store.commands_for(workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.command["definition_id"],
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID
    );
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_runner::TaskId::from_str(task_id))
            .await?
            .is_none(),
        "workflow runtime PR feedback must not register legacy task rows"
    );
    Ok(())
}

async fn assert_runtime_prompt_submission(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<String> {
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_runner::TaskId::from_str(task_id))
            .await?
            .is_none(),
        "workflow runtime prompt submissions must not register legacy task rows"
    );
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let instance = store
        .get_instance_by_task_id(task_id)
        .await?
        .expect("runtime prompt workflow should be persisted");
    assert_eq!(instance.definition_id, "prompt_task");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id);
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    assert_eq!(
        instance.data["project_id"],
        project_root.canonicalize()?.to_string_lossy().as_ref()
    );
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    Ok(instance.id)
}

fn webhook_signature(secret: &str, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("valid hmac key");
    mac.update(payload);
    let digest = mac.finalize().into_bytes();
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={digest_hex}")
}

#[derive(serde::Deserialize, Debug)]
struct HealthResponse {
    status: String,
    tasks: u64,
    persistence: PersistenceBlock,
    runtime_logs: RuntimeLogsBlock,
}

#[derive(serde::Deserialize, Debug)]
struct PersistenceBlock {
    degraded_subsystems: Vec<String>,
    runtime_state_dirty: bool,
    startup: StartupBlock,
}

#[derive(serde::Deserialize, Debug)]
struct StartupBlock {
    stores: Vec<StoreHealth>,
}

#[derive(serde::Deserialize, Debug)]
struct StoreHealth {
    name: String,
    critical: bool,
    ready: bool,
    error: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
struct RuntimeLogsBlock {
    state: String,
    path_hint: Option<String>,
    retention_days: u32,
}

async fn call_health(state: Arc<AppState>) -> anyhow::Result<HealthResponse> {
    use http_body_util::BodyExt;
    let app = Router::new()
        .route("/health", get(health_check))
        .with_state(state);
    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

#[tokio::test]
async fn health_endpoint_returns_ok_and_task_count() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let health = call_health(state).await?;
    assert_eq!(health.status, "ok");
    assert_eq!(health.tasks, 0);
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert!(!health.persistence.runtime_state_dirty);
    assert!(health.persistence.startup.stores.is_empty());
    assert_eq!(health.runtime_logs.state, "disabled");
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_subsystem_missing() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["q_value_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.degraded_subsystems, ["q_value_store"]);
    assert!(!health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_returns_nested_runtime_details() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
    }));
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123")
    .with_parent(parent.id.clone())
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
    }));
    store.upsert_instance(&parent).await?;
    store.upsert_instance(&child).await?;
    let event = store
        .append_event(
            &child.id,
            "PlanIssueRaised",
            "workflow-runtime-test",
            serde_json::json!({ "issue_number": 123 }),
        )
        .await?;
    let decision = harness_workflow::runtime::WorkflowDecision::new(
        child.id.clone(),
        "replanning",
        "run_replan",
        "replanning",
        "Replan requested after the budget was exhausted.",
    );
    let rejected = harness_workflow::runtime::WorkflowDecisionRecord::rejected(
        decision,
        Some(event.id),
        "replan limit exhausted",
    );
    store.record_decision(&rejected).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-2",
    );
    let command_id = store
        .enqueue_command(&child.id, Some(&rejected.id), &command)
        .await?;
    let not_before = chrono::Utc::now() + chrono::Duration::minutes(5);
    let runtime_job = store
        .enqueue_runtime_job_with_not_before(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-high",
            serde_json::json!({ "workflow_id": child.id }),
            Some(not_before),
        )
        .await?;
    let prompt_packet_digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    store
        .record_runtime_event(
            &runtime_job.id,
            "RuntimePromptPrepared",
            serde_json::json!({
                "prompt_packet_digest": prompt_packet_digest,
            }),
        )
        .await?;
    store
        .record_runtime_event(
            &runtime_job.id,
            "ActivityResultReady",
            serde_json::json!({ "status": "succeeded" }),
        )
        .await?;

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 2);
    assert_eq!(body["workflows"][0]["workflow"]["id"], "repo-backlog");
    let child_node = &body["workflows"][0]["children"][0];
    assert_eq!(child_node["workflow"]["id"], "issue-123");
    assert_eq!(
        child_node["decisions"][0]["rejection_reason"],
        "replan limit exhausted"
    );
    assert_eq!(
        child_node["commands"][0]["command"]["command"]["activity"],
        "replan_issue"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["runtime_profile"],
        "codex-high"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["not_before"],
        serde_json::json!(not_before)
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["runtime_event_count"],
        2
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["latest_runtime_event_type"],
        "ActivityResultReady"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["prompt_packet_digest"],
        prompt_packet_digest
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_enqueues_runtime_jobs() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-a");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: codex-high\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 123,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("replan_issue", "replan-1");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "codex-high",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 1);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 0);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].runtime_profile, "codex-high");
    let workflow_cfg = harness_core::config::workflow::load_workflow_config(&project_root)?;
    let expected_profile_manifest = super::background::runtime_profile_manifest_definition(
        &project_root,
        &workflow_cfg.runtime_dispatch,
    )?;
    let persisted_profile_manifest = store
        .get_definition(
            &expected_profile_manifest.id,
            expected_profile_manifest.version,
        )
        .await?
        .expect("runtime profile manifest definition should be persisted");
    assert_eq!(
        persisted_profile_manifest.metadata["default_profile"]["name"],
        "codex-high"
    );
    assert_eq!(
        persisted_profile_manifest.source_path,
        Some(
            project_root
                .join("WORKFLOW.md")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "dispatched"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_tick_enqueues_runtime_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:226"),
    )
    .with_id("issue-226")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-226",
    }));
    store.upsert_instance(&workflow).await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 0);
    assert_eq!(tick.rejected, 0);
    let updated = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "awaiting_feedback");
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        commands[0].command.command["definition_id"],
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID
    );
    assert_eq!(
        commands[0].command.command["child_activity"],
        harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY
    );
    Ok(())
}

#[tokio::test]
async fn runtime_repo_backlog_poll_tick_enqueues_runtime_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-backlog");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nrepo_backlog:\n  enabled: true\n  batch_limit: 5\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "owner/repo".to_string(),
            label: "harness".to_string(),
            project_root: Some(project_root.to_string_lossy().into_owned()),
        }],
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &project_root,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");

    let tick = super::background::run_runtime_repo_backlog_poll_tick(&state, 10).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 0);
    assert_eq!(tick.rejected, 0);
    let instances = store
        .list_instances_by_definition(
            harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
            None,
            None,
        )
        .await?;
    assert_eq!(instances.len(), 1);
    let workflow_id = instances[0].id.clone();
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("repo backlog workflow should exist");
    assert_eq!(instance.state, "scanning");
    assert_eq!(instance.data["label"], "harness");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::REPO_BACKLOG_POLL_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_limit_ignores_skipped_workflows() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-limit");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let valid_workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:227"),
    )
    .with_id("issue-227")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 227,
        "pr_number": 78,
        "pr_url": "https://github.com/owner/repo/pull/78",
        "task_id": "runtime-task-227",
    }));
    store.upsert_instance(&valid_workflow).await?;
    let skipped_workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:228"),
    )
    .with_id("issue-228")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 228,
        "task_id": "runtime-task-228",
    }));
    store.upsert_instance(&skipped_workflow).await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 1).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.skipped, 1);
    assert_eq!(store.commands_for(&valid_workflow.id).await?.len(), 1);
    assert!(store.commands_for(&skipped_workflow.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_respects_project_runtime_policy() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-disabled-runtime");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: false\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:229"),
    )
    .with_id("issue-229")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 229,
        "pr_number": 79,
        "pr_url": "https://github.com/owner/repo/pull/79",
        "task_id": "runtime-task-229",
    }));
    store.upsert_instance(&workflow).await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

    assert_eq!(tick.requested, 0);
    assert_eq!(tick.skipped, 1);
    assert!(store.commands_for(&workflow.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_uses_command_project_policy_when_server_root_disabled(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let server_root = dir.path().join("server-root");
    let project_root = dir.path().join("project-root");
    std::fs::create_dir(&server_root)?;
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        server_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: false\n  runtime_profile: server-disabled\nruntime_worker:\n  enabled: false\n---\n",
    )?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: project-runtime\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &server_root,
        harness_core::config::HarnessConfig::default(),
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:224"),
    )
    .with_id("issue-224")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 224,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-224");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-disabled",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 1);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 0);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].runtime_profile, "project-runtime");
    assert_eq!(
        store.commands_for(&workflow.id).await?[0].status,
        "dispatched"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_command_dispatch_tick_skips_when_command_project_runtime_disabled(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-root");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\n  runtime_profile: project-runtime\nruntime_worker:\n  enabled: false\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:225"),
    )
    .with_id("issue-225")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 225,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-225");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;

    let tick = super::background::run_runtime_command_dispatch_tick(
        &state,
        harness_workflow::runtime::RuntimeProfile::new(
            "server-fallback",
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        ),
        10,
    )
    .await?;

    assert_eq!(tick.enqueued, 0);
    assert_eq!(tick.already_dispatched, 0);
    assert_eq!(tick.skipped, 1);
    assert!(store
        .runtime_jobs_for_command(&command_id)
        .await?
        .is_empty());
    assert_eq!(store.commands_for(&workflow.id).await?[0].status, "skipped");
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_tick_runs_registered_agent_and_completes_job() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let agent = RuntimeStreamAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent.clone());
    let state =
        make_test_state_with_workflow_runtime_and_registry(dir.path(), &project_root, registry)
            .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:124"),
    )
    .with_id("issue-124")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 124,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-1");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    runtime_profile.model = Some("gpt-runtime".to_string());
    runtime_profile.reasoning_effort = Some("medium".to_string());
    runtime_profile.sandbox = Some("read-only".to_string());
    runtime_profile.approval_policy = Some("on-request".to_string());
    runtime_profile.timeout_secs = Some(300);
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": workflow.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key,
                "command": command.command,
                "runtime_profile": runtime_profile,
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    assert!(!tick.idle);
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Succeeded
    );
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "implement_issue");
    assert_eq!(output.summary, "runtime done");
    let events = store.runtime_events_for(&runtime_job.id).await?;
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[1].event_type, "RuntimeTurnStarted");
    assert_eq!(events[2].event_type, "RuntimePromptPrepared");
    let prompt_event = &events[2];
    assert_eq!(
        prompt_event.event["prompt_packet"]["schema"],
        "harness.runtime.prompt_packet.v1"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["required_structured_output"]["validation_commands"],
        "Validation commands run and their results."
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["schema"],
        "harness.runtime.activity_result.v1"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["activity"],
        "implement_issue"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["allowed_error_kinds"][1],
        "timeout"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["allowed_error_kinds"][2],
        "fatal"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["transition_contract"]
            ["on_succeeded"]["reducer_next_state"],
        "unchanged_until_pr_detected"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["agent_summary_contract"]
            ["must_include"][2],
        "PR URL or blocker"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["agent_summary_contract"]
            ["artifacts"]["pull_request"]["fields"][1],
        "pr_url"
    );
    assert_eq!(
        prompt_event.event["prompt_packet"]["activity_result_schema"]["optional_artifacts"]
            ["workflow_decision"]["allowed_confidence"][2],
        "high"
    );
    let decision_contract = &prompt_event.event["prompt_packet"]["activity_result_schema"]
        ["workflow_decision_contract"];
    assert_eq!(decision_contract["workflow_id"], "issue-124");
    assert_eq!(decision_contract["observed_state"], "implementing");
    assert!(decision_contract["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "pr_open"));
    let prompt_packet_digest = prompt_event.event["prompt_packet_digest"]
        .as_str()
        .expect("prompt packet digest should be recorded");
    assert_eq!(prompt_packet_digest.len(), 64);
    assert_eq!(events[3].event_type, "ActivityResultReady");
    let prompt_artifact = output
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "runtime_prompt_packet")
        .expect("runtime output should reference the prompt packet");
    assert_eq!(prompt_artifact.artifact["digest"], prompt_packet_digest);
    let prompts = agent.prompts.lock().await;
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("You are executing a Harness workflow runtime job."));
    assert!(prompts[0].contains("Activity: implement_issue"));
    assert!(prompts[0].contains("Prompt packet:"));
    assert!(prompts[0].contains("activity_result_schema"));
    assert!(prompts[0].contains("required_structured_output"));
    assert!(prompts[0].contains("gpt-runtime"));
    drop(prompts);
    let models = agent.models.lock().await;
    assert_eq!(models.as_slice(), &[Some("gpt-runtime".to_string())]);
    drop(models);
    let reasoning_efforts = agent.reasoning_efforts.lock().await;
    assert_eq!(reasoning_efforts.as_slice(), &[Some("medium".to_string())]);
    drop(reasoning_efforts);
    let sandbox_modes = agent.sandbox_modes.lock().await;
    assert_eq!(sandbox_modes.as_slice(), &[Some(SandboxMode::ReadOnly)]);
    drop(sandbox_modes);
    let approval_policies = agent.approval_policies.lock().await;
    assert_eq!(
        approval_policies.as_slice(),
        &[Some("on-request".to_string())]
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_starts_child_workflow_without_agent_turn() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        "issue:126",
        "repo-backlog:owner/repo:issue:126:start",
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    runtime_profile.max_turns = Some(0);
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
                "runtime_profile": runtime_profile,
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 126);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created");
    assert_eq!(child.state, "discovered");
    assert_eq!(child.parent_workflow_id.as_deref(), Some("repo-backlog"));
    assert_eq!(child.data["issue_number"], 126);
    assert_eq!(child.data["started_by_runtime_job_id"], runtime_job.id);
    let parent_after = store
        .get_instance("repo-backlog")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "start_child_workflow");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&parent.id, None)
            .await?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_starts_pr_feedback_child_workflow_without_agent_turn(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-pr-feedback-child");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:226"),
    )
    .with_id("issue-pr-feedback-parent")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-226",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "pr-feedback-sweep:issue-226:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            "subject_key": "pr:77",
            "child_activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "repo": "owner/repo",
            "issue_number": 226,
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": command.runtime_activity_key(),
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(
        store
            .runtime_turns_started_for_workflow(&parent.id, None)
            .await?,
        0
    );
    let children = store
        .list_instances_by_definition(
            harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            Some(&project_id),
            None,
        )
        .await?;
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(child.state, "inspecting");
    assert_eq!(
        child.parent_workflow_id.as_deref(),
        Some("issue-pr-feedback-parent")
    );
    assert_eq!(child.data["pr_number"], 77);
    assert_eq!(child.data["started_by_runtime_job_id"], runtime_job.id);
    let child_commands = store.commands_for(&child.id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some(harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_requeues_pr_feedback_child_inspect_after_stale_dedupe(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-pr-feedback-child-retry");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:227"),
    )
    .with_id("issue-pr-feedback-parent-retry")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 227,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-227",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "pr-feedback-sweep:issue-227:77",
        serde_json::json!({
            "definition_id": harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
            "subject_key": "pr:77",
            "child_activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "repo": "owner/repo",
            "issue_number": 227,
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": command.runtime_activity_key(),
                "command": command.command.clone(),
            }),
        )
        .await?;
    let child_id = format!("{}::pr-feedback:{}", parent.id, command_id);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
        1,
        "pending",
        harness_workflow::runtime::WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id(child_id.clone())
    .with_parent(parent.id.clone());
    store.upsert_instance(&child).await?;
    let stale_dedupe_key = format!("pr-feedback-child:{}:inspect", child_id);
    let stale_command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
        stale_dedupe_key.clone(),
    );
    let stale_command_id = store
        .enqueue_command(&child_id, None, &stale_command)
        .await?;
    store
        .mark_command_status(&stale_command_id, "completed")
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child.state, "inspecting");
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 2);
    let stale = child_commands
        .iter()
        .find(|record| record.id == stale_command_id)
        .expect("stale command should still be recorded");
    assert_eq!(stale.status, "completed");
    let requeued = child_commands
        .iter()
        .find(|record| record.id != stale_command_id)
        .expect("retry command should be recorded");
    assert_eq!(requeued.status, "pending");
    assert_eq!(
        requeued.command.activity_name(),
        Some(harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY)
    );
    assert!(requeued
        .command
        .dedupe_key
        .starts_with(&format!("{}:retry:", stale_dedupe_key)));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_auto_submits_repo_backlog_child_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-auto-submit")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:127:start",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:127",
            "repo": "owner/repo",
            "labels": ["harness"],
            "source": "github",
            "external_id": "127",
            "auto_submit": true,
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 127);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created");
    assert_eq!(child.state, "implementing");
    assert_eq!(child.data["source"], "github");
    assert_eq!(child.data["external_id"], "127");
    let child_commands = store.commands_for(&child_id).await?;
    assert_eq!(child_commands.len(), 1);
    assert_eq!(
        child_commands[0].command.activity_name(),
        Some("implement_issue")
    );
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert!(output
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "child_submission"));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_auto_submits_repo_backlog_child_with_dependencies() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-auto-submit-deps");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-auto-submit-deps")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
    }));
    store.upsert_instance(&parent).await?;
    let command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow,
        "repo-backlog:owner/repo:issue:128:start",
        serde_json::json!({
            "definition_id": "github_issue_pr",
            "subject_key": "issue:128",
            "repo": "owner/repo",
            "labels": ["harness"],
            "depends_on": [127],
            "source": "github",
            "external_id": "128",
            "auto_submit": true,
        }),
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 128);
    let child = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should be created");
    assert_eq!(child.state, "awaiting_dependencies");
    assert_eq!(
        child.data["depends_on"][0],
        "repo-backlog:owner/repo:issue:127"
    );
    let child_commands = store.commands_for(&child_id).await?;
    assert!(
        child_commands.is_empty(),
        "dependent issue should not enqueue implement_issue until dependencies release"
    );
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert!(output
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == "child_submission"));
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_marks_bound_issue_done_without_agent_turn() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "reconciling",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-mark")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "last_issue_number": 127,
        "last_pr_number": 77,
        "last_pr_url": "https://github.com/owner/repo/pull/77",
    }));
    store.upsert_instance(&parent).await?;
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 127);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:127"),
    )
    .with_id(child_id.clone())
    .with_parent("repo-backlog-mark")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 127,
    }));
    store.upsert_instance(&child).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "mark_bound_issue_done",
        "repo-backlog:owner/repo:pr:77:merged",
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child_after.state, "done");
    assert_eq!(
        child_after.parent_workflow_id.as_deref(),
        Some("repo-backlog-mark")
    );
    assert_eq!(child_after.data["issue_number"], 127);
    assert_eq!(child_after.data["pr_number"], 77);
    assert_eq!(
        child_after.data["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    assert_eq!(
        child_after.data["started_by_runtime_job_id"],
        runtime_job.id
    );
    let parent_after = store
        .get_instance("repo-backlog-mark")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "mark_bound_issue_done");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_recovers_stale_issue_workflow_without_agent_turn() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "reconciling",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-recover")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "last_issue_number": 128,
        "last_active_task_id": "task-128",
        "last_observed_state": "implementing",
        "last_recovery_reason": "active task disappeared during startup reconciliation",
    }));
    store.upsert_instance(&parent).await?;
    let child_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 128);
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:128"),
    )
    .with_id(child_id.clone())
    .with_parent("repo-backlog-recover")
    .with_data(serde_json::json!({
        "project_id": project_id.clone(),
        "repo": "owner/repo",
        "issue_number": 128,
    }));
    store.upsert_instance(&child).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "recover_issue_workflow",
        "repo-backlog:owner/repo:issue:128:recover",
    );
    let command_id = store.enqueue_command(&parent.id, None, &command).await?;
    let activity = command.runtime_activity_key().to_string();
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": parent.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key.clone(),
                "activity": activity,
                "command": command.command.clone(),
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.succeeded, 1);
    assert_eq!(tick.failed, 0);
    assert_eq!(tick.cancelled, 0);
    let child_after = store
        .get_instance(&child_id)
        .await?
        .expect("child workflow should still exist");
    assert_eq!(child_after.state, "scheduled");
    assert_eq!(
        child_after.parent_workflow_id.as_deref(),
        Some("repo-backlog-recover")
    );
    assert_eq!(child_after.data["issue_number"], 128);
    assert_eq!(child_after.data["previous_state"], "implementing");
    assert_eq!(child_after.data["previous_active_task_id"], "task-128");
    assert_eq!(
        child_after.data["recovery_reason"],
        "active task disappeared during startup reconciliation"
    );
    assert_eq!(
        child_after.data["started_by_runtime_job_id"],
        runtime_job.id
    );
    let parent_after = store
        .get_instance("repo-backlog-recover")
        .await?
        .expect("parent workflow should still exist");
    assert_eq!(parent_after.state, "idle");
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "recover_issue_workflow");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_worker_applies_runtime_profile_timeout() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", BlockingAgent::new());
    let state =
        make_test_state_with_workflow_runtime_and_registry(dir.path(), &project_root, registry)
            .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:125"),
    )
    .with_id("issue-125")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 125,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("implement_issue", "impl-2");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let mut runtime_profile = harness_workflow::runtime::RuntimeProfile::new(
        "codex-default",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
    );
    runtime_profile.timeout_secs = Some(1);
    let runtime_job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-default",
            serde_json::json!({
                "workflow_id": workflow.id,
                "command_id": command_id,
                "command_type": command.command_type,
                "dedupe_key": command.dedupe_key,
                "command": command.command,
                "runtime_profile": runtime_profile,
            }),
        )
        .await?;

    let tick = crate::workflow_runtime_worker::run_runtime_job_worker_tick(
        &state,
        "worker-test",
        chrono::Duration::minutes(5),
    )
    .await?;

    assert_eq!(tick.failed, 1);
    assert_eq!(tick.succeeded, 0);
    assert_eq!(tick.cancelled, 0);
    assert!(!tick.idle);
    let completed = store
        .get_runtime_job(&runtime_job.id)
        .await?
        .expect("runtime job should exist");
    assert_eq!(
        completed.status,
        harness_workflow::runtime::RuntimeJobStatus::Failed
    );
    let output: harness_workflow::runtime::ActivityResult = serde_json::from_value(
        completed
            .output
            .expect("activity result should be recorded"),
    )?;
    assert_eq!(output.activity, "implement_issue");
    assert_eq!(
        output.status,
        harness_workflow::runtime::ActivityStatus::Failed
    );
    assert!(
        output
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out")),
        "failed runtime job should include timeout error: {output:?}"
    );
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_runtime_state_dirty() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    state.runtime_state_dirty.store(true, Ordering::Release);
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert!(health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_runtime_logs_redacts_absolute_path() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    server.runtime_logs = crate::server::RuntimeLogMetadata::enabled(
        dir.path()
            .join("logs/harness-serve-20260430T120000Z-pid1.log"),
        45,
    );

    let health = call_health(state).await?;
    assert_eq!(health.runtime_logs.state, "enabled");
    assert_eq!(
        health.runtime_logs.path_hint.as_deref(),
        Some("logs/harness-serve-20260430T120000Z-pid1.log")
    );
    assert_eq!(health.runtime_logs.retention_days, 45);
    assert!(
        !format!("{health:?}").contains(&dir.path().display().to_string()),
        "health response must not expose an absolute runtime log path"
    );
    Ok(())
}

#[tokio::test]
async fn health_runtime_logs_can_report_degraded_without_raw_error_text() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).expect("unique state");
    let server = Arc::get_mut(&mut state_mut.core.server).expect("unique server");
    server.runtime_logs = crate::server::RuntimeLogMetadata::degraded(
        Some("logs/harness-serve-20260430T120000Z-pid1.log".to_string()),
        7,
    );

    let health = call_health(state).await?;
    assert_eq!(health.runtime_logs.state, "degraded");
    assert_eq!(
        health.runtime_logs.path_hint.as_deref(),
        Some("logs/harness-serve-20260430T120000Z-pid1.log")
    );
    assert_eq!(health.runtime_logs.retention_days, 7);
    assert!(!format!("{health:?}").contains("permission denied"));
    Ok(())
}

#[tokio::test]
async fn health_startup_errors_are_redacted() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    let state_mut = Arc::get_mut(&mut state).unwrap();
    state_mut.degraded_subsystems = vec!["review_store"];
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("review_store")
                .failed("failed to connect to postgres://user:secret@db.internal/harness"),
        ];

    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.startup.stores.len(), 1);
    let store = &health.persistence.startup.stores[0];
    assert_eq!(store.name, "review_store");
    assert!(!store.critical);
    assert!(!store.ready);
    assert_eq!(store.error.as_deref(), Some("database_unavailable"));
    assert!(
        !format!("{health:?}").contains("secret"),
        "health response must not expose raw startup error text"
    );
    Ok(())
}

#[tokio::test]
async fn health_degraded_multiple_subsystems() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().startup_statuses = vec![
        crate::http::state::StoreStartupResult::optional("q_value_store")
            .failed("pool timed out while waiting for an open connection"),
        crate::http::state::StoreStartupResult::optional("runtime_state_store")
            .failed("runtime state snapshot restore failed"),
    ];
    Arc::get_mut(&mut state).unwrap().degraded_subsystems =
        vec!["q_value_store", "runtime_state_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["q_value_store", "runtime_state_store"]
    );
    assert_eq!(health.persistence.startup.stores.len(), 2);
    Ok(())
}

#[tokio::test]
async fn health_degraded_both_conditions() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering;
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["workspace_manager"];
    state.runtime_state_dirty.store(true, Ordering::Release);
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["workspace_manager"]
    );
    assert!(health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn health_reports_critical_store_failure_details() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().startup_statuses = vec![
        crate::http::state::StoreStartupResult::critical("event_store")
            .failed("failed to open Postgres bootstrap pool"),
    ];
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["event_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.startup.stores.len(), 1);
    assert_eq!(health.persistence.startup.stores[0].name, "event_store");
    assert!(health.persistence.startup.stores[0].critical);
    assert!(!health.persistence.startup.stores[0].ready);
    Ok(())
}

#[tokio::test]
async fn token_usage_route_is_registered() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = token_usage_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/token-usage")
                .body(Body::empty())?,
        )
        .await?;

    assert_ne!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response.status() == StatusCode::OK
            || response.status() == StatusCode::INTERNAL_SERVER_ERROR
    );
    Ok(())
}

#[tokio::test]
async fn webhook_issue_mention_schedules_runtime_issue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "majiayu000/harness".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_issue_submission(
        &state,
        dir.path(),
        Some("majiayu000/harness"),
        106,
        json["task_id"].as_str().expect("task id should be present"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn webhook_review_on_pr_requests_runtime_pr_feedback() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "majiayu000/harness".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let (workflow_id, runtime_task_id) =
        seed_bound_runtime_pr_workflow(&state, dir.path(), "majiayu000/harness", 42, 42).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 42, "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" } },
        "comment": { "body": "@harness review" },
        "repository": { "full_name": "majiayu000/harness" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "awaiting_feedback");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_eq!(json["task_id"], runtime_task_id);
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_pr_feedback_requested(&state, &workflow_id, &runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_fix_ci_on_pr_creates_runtime_prompt_submission() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "majiayu000/harness".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "created",
        "issue": {
            "number": 42,
            "html_url": "https://github.com/majiayu000/harness/pull/42",
            "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" }
        },
        "comment": {
            "body": "@harness fix CI",
            "html_url": "https://github.com/majiayu000/harness/issues/42#issuecomment-1"
        },
        "repository": { "full_name": "majiayu000/harness" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    let runtime_task_id = json["task_id"].as_str().expect("task id should be present");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_prompt_submission(&state, dir.path(), runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_secret_requires_signature_header() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn webhook_secret_rejects_invalid_signature_value() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header(
                    "x-hub-signature-256",
                    "sha256=0000000000000000000000000000000000000000000000000000000000000000",
                )
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn webhook_empty_secret_configuration_fails_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("")).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

#[tokio::test]
async fn webhook_missing_secret_configuration_fails_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), None).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    Ok(())
}

#[tokio::test]
async fn webhook_rejects_invalid_event_header() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let app = webhook_app(state);

    let payload = serde_json::json!({
        "action": "created",
        "issue": { "number": 106 },
        "comment": { "body": "@harness please handle this issue" },
        "repository": { "full_name": "majiayu000/harness" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "Issue-Comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn webhook_body_limit_rejects_large_payload() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let body_limit = state.core.server.config.server.max_webhook_body_bytes;
    let app = webhook_app(state);

    let oversized = vec![b'a'; body_limit + 1024];
    let signature = webhook_signature(secret, &oversized);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(oversized))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

#[tokio::test]
async fn create_task_with_prompt_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let before_count = state.core.tasks.count();
    let app = task_app(state.clone());

    let body = serde_json::json!({ "prompt": "fix the bug" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = response_json(response).await?;
    assert!(
        resp["error"].as_str().is_some_and(
            |error| error.contains("workflow runtime store is required for prompt submissions")
        ),
        "unexpected response: {resp}"
    );
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn create_task_with_prompt_returns_workflow_runtime_submission() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "fix the prompt-only bug",
        "source": "dashboard",
        "external_id": "manual:prompt:http"
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let resp = response_json(response).await?;
    assert!(resp["task_id"].is_string());
    assert_eq!(resp["status"], "implementing");
    assert_eq!(resp["execution_path"], "workflow_runtime");
    let task_id = task_runner::TaskId::from_str(resp["task_id"].as_str().unwrap());
    assert!(state
        .core
        .tasks
        .get_with_db_fallback(&task_id)
        .await?
        .is_none());
    assert_eq!(state.core.tasks.count(), before_count);

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = crate::workflow_runtime_submission::prompt_workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("manual:prompt:http"),
        &task_id,
    );
    assert_eq!(resp["workflow_id"], workflow_id);
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.definition_id, "prompt_task");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert!(instance.data.get("prompt").is_none());
    assert_eq!(instance.data["prompt_summary"], "prompt task");
    assert_eq!(
        instance.data["prompt_chars"],
        "fix the prompt-only bug".chars().count()
    );
    let prompt_ref = instance.data["prompt_ref"]
        .as_str()
        .expect("prompt ref should be persisted");
    assert_eq!(
        crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref).as_deref(),
        Some("fix the prompt-only bug")
    );
    assert_eq!(instance.data["source"], "dashboard");
    assert_eq!(instance.data["external_id"], "manual:prompt:http");
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    assert!(commands[0].command.command.get("prompt").is_none());
    assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);

    let detail_response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task_id.as_str()))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail = response_json(detail_response).await?;
    assert_eq!(detail["task_kind"], "prompt");
    assert_eq!(detail["status"], "implementing");
    assert_eq!(detail["execution_path"], "workflow_runtime");
    assert_eq!(detail["workflow_id"], workflow_id);
    assert_eq!(detail["workflow"]["id"], workflow_id);
    assert_eq!(detail["workflow"]["definition_id"], "prompt_task");
    assert_eq!(detail["workflow"]["state"], "implementing");
    assert_eq!(
        detail["workflow"]["project_id"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert!(detail["workflow"].get("data").is_none());
    Ok(())
}

#[tokio::test]
async fn create_task_with_issue_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let before_count = state.core.tasks.count();
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "repo": "owner/repo",
        "issue": 42,
        "labels": ["bug"]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = response_json(response).await?;
    assert!(
        resp["error"].as_str().is_some_and(|error| error
            .contains("workflow runtime store is required for GitHub issue submissions")),
        "unexpected response: {resp}"
    );
    assert_eq!(state.core.tasks.count(), before_count);

    Ok(())
}

#[tokio::test]
async fn create_task_with_issue_returns_workflow_runtime_submission() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "repo": "owner/repo",
        "issue": 42,
        "labels": ["bug"]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let resp = response_json(response).await?;
    assert!(resp["task_id"].is_string());
    assert_eq!(resp["status"], "implementing");
    assert_eq!(resp["execution_path"], "workflow_runtime");
    let task_id = task_runner::TaskId::from_str(resp["task_id"].as_str().unwrap());
    assert!(state
        .core
        .tasks
        .get_with_db_fallback(&task_id)
        .await?
        .is_none());
    assert_eq!(state.core.tasks.count(), before_count);

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        42,
    );
    assert_eq!(resp["workflow_id"], workflow_id);
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
    Ok(())
}

#[tokio::test]
async fn list_tasks_includes_runtime_issue_submissions() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = Router::new()
        .route("/tasks", get(list_tasks).post(task_routes::create_task))
        .with_state(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "repo": "owner/repo",
        "issue": 52,
        "labels": ["bug"]
    });
    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let created = response_json(create_response).await?;
    let task_id = created["task_id"]
        .as_str()
        .expect("runtime submission should return a task handle")
        .to_string();
    assert_eq!(state.core.tasks.count(), before_count);

    let list_response = app
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed = response_json(list_response).await?;
    let tasks = listed.as_array().expect("tasks should be an array");
    let runtime_task = tasks
        .iter()
        .find(|task| task["id"] == task_id)
        .expect("runtime issue submission should be listed");
    let canonical_project_root = project_root.canonicalize()?;

    assert_eq!(runtime_task["task_kind"], "issue");
    assert_eq!(runtime_task["status"], "implementing");
    assert_eq!(runtime_task["external_id"], "issue:52");
    assert_eq!(runtime_task["repo"], "owner/repo");
    assert_eq!(runtime_task["description"], "issue #52");
    assert_eq!(
        runtime_task["project"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert_eq!(runtime_task["scheduler"]["authority_state"], "running");
    assert!(runtime_task["workflow"]["id"]
        .as_str()
        .is_some_and(|id| id.ends_with("::repo:owner/repo::issue:52")));
    assert_eq!(runtime_task["workflow"]["definition_id"], "github_issue_pr");
    assert_eq!(runtime_task["workflow"]["state"], "implementing");
    assert_eq!(runtime_task["workflow"]["issue_number"], 52);
    Ok(())
}

#[tokio::test]
async fn merge_task_accepts_runtime_workflow_task_handle() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:53"),
    )
    .with_id("runtime-ready-53")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 53,
        "pr_number": 125,
        "pr_url": "https://github.com/owner/repo/pull/125",
        "task_id": "runtime-ready-task",
    }));
    store.upsert_instance(&workflow).await?;
    let app = Router::new()
        .route("/tasks/{id}/merge", post(task_mutation_routes::merge_task))
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/runtime-ready-task/merge")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "merge_approved");
    assert_eq!(body["execution_path"], "workflow_runtime");
    let updated = store
        .get_instance("runtime-ready-53")
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "done");
    assert_eq!(updated.data["last_decision"], "approve_merge");
    assert_eq!(updated.data["merge_approved_task_id"], "runtime-ready-task");
    let events = store.events_for("runtime-ready-53").await?;
    assert!(events
        .iter()
        .any(|event| event.event_type == "MergeApproved"));
    let decisions = store.decisions_for("runtime-ready-53").await?;
    assert!(decisions
        .iter()
        .any(|record| record.accepted && record.decision.decision == "approve_merge"));
    let commands = store.commands_for("runtime-ready-53").await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.command_type,
        harness_workflow::runtime::WorkflowCommandType::MarkDone
    );
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_merge_endpoint_approves_ready_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "ready_to_merge",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:54"),
    )
    .with_id("runtime-ready-54")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 54,
        "pr_number": 126,
        "pr_url": "https://github.com/owner/repo/pull/126",
        "task_id": "runtime-ready-task-54",
    }));
    store.upsert_instance(&workflow).await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/merge",
            post(task_mutation_routes::merge_workflow_runtime),
        )
        .with_state(state.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/merge")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "workflow_id": "runtime-ready-54" }).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response_json(response).await?;
    assert_eq!(body["workflow_id"], "runtime-ready-54");
    let updated = store
        .get_instance("runtime-ready-54")
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "done");
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_cancel_endpoint_cancels_issue_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = harness_core::types::TaskId::from_str("runtime-cancel-task-55");
    let submission = crate::workflow_runtime_submission::record_issue_submission(
        store,
        crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
            project_root: &project_root,
            repo: Some("owner/repo"),
            issue_number: 55,
            task_id: &task_id,
            labels: &[],
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            source: None,
            external_id: None,
        },
    )
    .await?;
    let app = Router::new()
        .route(
            "/api/workflows/runtime/cancel",
            post(task_mutation_routes::cancel_workflow_runtime),
        )
        .with_state(state.clone());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/cancel")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "workflow_id": submission.workflow_id }).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["status"], "cancelled");
    assert_eq!(body["execution_path"], "workflow_runtime");
    let updated = store
        .get_instance(&submission.workflow_id)
        .await?
        .expect("workflow should still exist");
    assert_eq!(updated.state, "cancelled");
    assert_eq!(updated.data["last_decision"], "cancel_issue_submission");

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/cancel")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "workflow_id": submission.workflow_id }).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = response_json(response).await?;
    assert_eq!(body["error"], "workflow already terminal");
    assert_eq!(body["state"], "cancelled");
    Ok(())
}

#[tokio::test]
async fn list_tasks_includes_runtime_prompt_submissions() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = Router::new()
        .route("/tasks", get(list_tasks).post(task_routes::create_task))
        .with_state(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "fix listed runtime prompt",
        "source": "dashboard",
        "external_id": "manual:prompt:list"
    });
    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let created = response_json(create_response).await?;
    let task_id = created["task_id"]
        .as_str()
        .expect("runtime submission should return a task handle")
        .to_string();
    assert_eq!(state.core.tasks.count(), before_count);

    let list_response = app
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(list_response.status(), StatusCode::OK);
    let listed = response_json(list_response).await?;
    let tasks = listed.as_array().expect("tasks should be an array");
    let runtime_task = tasks
        .iter()
        .find(|task| task["id"] == task_id)
        .expect("runtime prompt submission should be listed");
    let canonical_project_root = project_root.canonicalize()?;

    assert_eq!(runtime_task["task_kind"], "prompt");
    assert_eq!(runtime_task["status"], "implementing");
    assert_eq!(runtime_task["source"], "dashboard");
    assert_eq!(runtime_task["external_id"], "manual:prompt:list");
    assert_eq!(runtime_task["description"], "prompt task");
    assert_eq!(
        runtime_task["project"],
        canonical_project_root.to_string_lossy().as_ref()
    );
    assert_eq!(runtime_task["scheduler"]["authority_state"], "running");
    assert_eq!(runtime_task["workflow"]["id"], created["workflow_id"]);
    assert_eq!(runtime_task["workflow"]["definition_id"], "prompt_task");
    assert_eq!(runtime_task["workflow"]["state"], "implementing");
    Ok(())
}

#[tokio::test]
async fn create_task_with_blocked_issue_returns_runtime_state() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = task_app(state.clone());

    let body = serde_json::json!({
        "project": project_root.display().to_string(),
        "repo": "owner/repo",
        "issue": 43,
        "depends_on": ["missing-dependency"]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let resp = response_json(response).await?;
    assert!(resp["task_id"].is_string());
    assert_eq!(resp["status"], "awaiting_dependencies");
    assert_eq!(resp["execution_path"], "workflow_runtime");

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        43,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.state, "awaiting_dependencies");
    assert_eq!(
        instance.data["depends_on"],
        serde_json::json!(["missing-dependency"])
    );
    Ok(())
}

#[tokio::test]
async fn create_task_empty_request_returns_bad_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let app = task_app(state);

    let body = serde_json::json!({});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn get_task_returns_not_found_for_missing_id() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = task_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks/nonexistent-id")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn create_then_get_task_returns_state() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;

    let create_body = serde_json::json!({
        "project": project_root.display().to_string(),
        "prompt": "add tests",
    });
    let create_resp = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(create_body.to_string()))?,
        )
        .await?;
    assert_eq!(create_resp.status(), StatusCode::ACCEPTED);

    let create_json = response_json(create_resp).await?;
    let task_id = create_json["task_id"]
        .as_str()
        .expect("task_id should be string");
    assert_eq!(create_json["status"], "implementing");
    assert_eq!(create_json["execution_path"], "workflow_runtime");

    let get_resp = task_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{task_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_resp.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let get_body = get_resp.into_body().collect().await?.to_bytes();
    let task_json: serde_json::Value = serde_json::from_slice(&get_body)?;
    assert_eq!(task_json["id"], task_id);
    assert_eq!(task_json["status"], "implementing");
    assert_eq!(task_json["execution_path"], "workflow_runtime");
    assert_eq!(task_json["workflow"]["definition_id"], "prompt_task");
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_issues_returns_runtime_submissions() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "issues": [42, 43],
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    for (entry, issue_number) in entries.iter().zip([42_u64, 43]) {
        assert_eq!(entry["status"], "implementing");
        assert_eq!(entry["execution_path"], "workflow_runtime");
        let workflow_id = assert_runtime_issue_submission(
            &state,
            &project_root,
            None,
            issue_number,
            entry["task_id"]
                .as_str()
                .expect("task id should be present"),
        )
        .await?;
        assert_eq!(entry["workflow_id"], workflow_id);
    }
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_prompts_returns_runtime_submissions() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "tasks": [
                            { "description": "batch prompt 1" },
                            { "description": "batch prompt 2" }
                        ]
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    for (entry, prompt) in entries.iter().zip(["batch prompt 1", "batch prompt 2"]) {
        assert_eq!(entry["status"], "implementing");
        assert_eq!(entry["execution_path"], "workflow_runtime");
        let task_id = task_runner::TaskId::from_str(
            entry["task_id"]
                .as_str()
                .expect("task id should be present"),
        );
        assert!(state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none());
        let instance = store
            .get_instance_by_task_id(task_id.as_str())
            .await?
            .expect("runtime prompt submission should be persisted");
        assert_eq!(entry["workflow_id"], instance.id);
        assert_eq!(instance.definition_id, "prompt_task");
        assert_eq!(instance.state, "implementing");
        assert!(instance.data.get("prompt").is_none());
        assert_eq!(instance.data["prompt_summary"], "prompt task");
        assert_eq!(instance.data["prompt_chars"], prompt.chars().count());
        let prompt_ref = instance.data["prompt_ref"]
            .as_str()
            .expect("prompt ref should be persisted");
        assert_eq!(
            crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref)
                .as_deref(),
            Some(prompt)
        );
        let commands = store.commands_for(&instance.id).await?;
        assert_eq!(commands.len(), 1);
        assert!(commands[0].command.command.get("prompt").is_none());
        assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);
    }
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_conflicting_runtime_prompts_adds_dependencies(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "tasks": [
                            { "description": "update src/lib.rs first" },
                            { "description": "update src/lib.rs second" }
                        ]
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["serialized"], true);
    assert_eq!(entries[1]["serialized"], true);
    assert_eq!(
        entries[0]["conflict_files"],
        serde_json::json!(["src/lib.rs"])
    );
    assert_eq!(
        entries[1]["conflict_files"],
        serde_json::json!(["src/lib.rs"])
    );
    assert_eq!(entries[0]["status"], "implementing");
    assert!(
        entries[1]["status"] == "awaiting_dependencies" || entries[1]["status"] == "implementing"
    );

    let first_task_id = entries[0]["task_id"]
        .as_str()
        .expect("first task id should be present");
    let second_task_id = entries[1]["task_id"]
        .as_str()
        .expect("second task id should be present");
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let first = store
        .get_instance_by_task_id(first_task_id)
        .await?
        .expect("first runtime prompt submission should be persisted");
    let second = store
        .get_instance_by_task_id(second_task_id)
        .await?
        .expect("second runtime prompt submission should be persisted");

    assert_eq!(
        second.data["depends_on"],
        serde_json::json!([first_task_id])
    );
    assert_eq!(second.data["required_depends_on"], serde_json::json!([]));
    assert_eq!(
        second.data["serialization_depends_on"],
        serde_json::json!([first_task_id])
    );
    assert_eq!(store.commands_for(&first.id).await?.len(), 1);
    let second_commands = store.commands_for(&second.id).await?.len();
    match second.state.as_str() {
        "awaiting_dependencies" => assert_eq!(second_commands, 0),
        "implementing" => assert_eq!(second_commands, 1),
        state => panic!("unexpected second runtime prompt state {state}"),
    }
    Ok(())
}

#[tokio::test]
async fn create_tasks_batch_with_issues_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;
    let before_count = state.core.tasks.count();
    let response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/batch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "issues": [42, 43],
                        "repo": "owner/repo",
                    })
                    .to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let batch_json = response_json(response).await?;
    let entries = batch_json
        .as_array()
        .expect("batch response should be an array");
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert!(
            entry["error"].as_str().is_some_and(|error| error
                .contains("workflow runtime store is required for GitHub issue submissions")),
            "unexpected entry: {entry}"
        );
    }
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn get_task_hides_internal_system_input_metadata() -> anyhow::Result<()> {
    use axum::response::IntoResponse;

    let dir = tempfile::tempdir()?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Review,
        status: task_runner::TaskStatus::ReviewWaiting,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("periodic_review".to_string()),
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: None,
        repo: Some("owner/repo".to_string()),
        description: Some("periodic review".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Review,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    let task_id = task.id.to_string();

    let response = axum::Json(task).into_response();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let task_json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(task_json["id"], task_id);
    assert!(task_json.get("system_input").is_none());
    Ok(())
}

#[tokio::test]
async fn get_task_includes_round_telemetry_and_failure() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task_id = task_runner::TaskId::new();
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.rounds.push(task_runner::RoundResult::new(
        1,
        "implement",
        "upstream_failure",
        Some("provider error".to_string()),
        Some(TurnTelemetry {
            first_token_latency_ms: Some(123),
            completed_latency_ms: Some(456),
            ..Default::default()
        }),
        Some(TurnFailure {
            kind: TurnFailureKind::Upstream,
            provider: Some("anthropic-api".to_string()),
            upstream_status: Some(500),
            message: Some("API returned 500".to_string()),
            body_excerpt: Some("{\"type\":\"error\"}".to_string()),
        }),
    ));
    state.core.tasks.insert(&task).await;

    let response = task_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    let task_json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(
        task_json["rounds"][0]["telemetry"]["first_token_latency_ms"],
        123
    );
    assert_eq!(task_json["rounds"][0]["failure"]["kind"], "upstream");
    assert_eq!(task_json["rounds"][0]["failure"]["upstream_status"], 500);
    Ok(())
}

#[tokio::test]
async fn closed_task_sse_replay_includes_observability_fields() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task_id = task_runner::TaskId::new();
    let mut task = task_runner::TaskState::new(task_id.clone());
    task.rounds.push(task_runner::RoundResult::new(
        1,
        "review",
        "timeout",
        Some("reviewer stalled".to_string()),
        Some(TurnTelemetry {
            completed_latency_ms: Some(900),
            ..Default::default()
        }),
        Some(TurnFailure {
            kind: TurnFailureKind::Timeout,
            provider: Some("claude".to_string()),
            upstream_status: None,
            message: Some("timeout".to_string()),
            body_excerpt: None,
        }),
    ));
    state.core.tasks.insert(&task).await;

    let app = Router::new()
        .route("/tasks/{id}/stream", get(stream_task_sse))
        .with_state(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}/stream", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    let body_text = String::from_utf8(body.to_vec())?;
    let mut saw_telemetry = false;
    let mut saw_failure = false;
    let mut saw_timeout_failure = false;
    for line in body_text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let item: StreamItem = serde_json::from_str(data)?;
        if let StreamItem::MessageDelta { text } = item {
            saw_telemetry |= text.contains("telemetry=");
            if let Some((_, failure_json)) = text.split_once("\nfailure=") {
                saw_failure = true;
                let failure: TurnFailure = serde_json::from_str(failure_json.trim())?;
                saw_timeout_failure |= failure.kind == TurnFailureKind::Timeout;
            }
        }
    }
    assert!(saw_telemetry);
    assert!(saw_failure);
    assert!(saw_timeout_failure);

    Ok(())
}

#[tokio::test]
async fn runtime_issue_sse_stream_keeps_active_workflow_open_without_done() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    init_fake_git_repo(&project_root)?;
    let state = make_test_state_with_workflow_runtime_and_registry(
        dir.path(),
        &project_root,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;

    let create_response = task_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "project": project_root.display().to_string(),
                        "repo": "owner/repo",
                        "issue": 44,
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let create_json = response_json(create_response).await?;
    let task_id = create_json["task_id"]
        .as_str()
        .expect("task id should be present");

    let response = Router::new()
        .route("/tasks/{id}/stream", get(stream_task_sse))
        .with_state(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{task_id}/stream"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    use http_body_util::BodyExt;
    let mut body = response.into_body();
    let mut body_text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !body_text.contains("[workflow]") && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    body_text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => anyhow::bail!("active runtime workflow stream closed before replay"),
            Err(_) => break,
        }
    }
    assert!(body_text.contains("[workflow]"));
    assert!(!body_text.contains("\"type\":\"done\""));
    assert!(!body_text.contains("\"type\":\"Done\""));

    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(100), body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    body_text.push_str(&String::from_utf8_lossy(&data));
                    assert!(!body_text.contains("\"type\":\"done\""));
                    assert!(!body_text.contains("\"type\":\"Done\""));
                }
            }
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => anyhow::bail!("active runtime workflow stream closed"),
            Err(_) => return Ok(()),
        }
    }

    anyhow::bail!("active runtime workflow stream did not become idle")
}

#[tokio::test]
async fn intake_status_returns_three_channels() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;

    let channels = json["channels"].as_array().expect("channels is array");
    assert_eq!(channels.len(), 3);
    let names: Vec<&str> = channels
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"github"));
    assert!(names.contains(&"feishu"));
    assert!(names.contains(&"dashboard"));
    Ok(())
}

#[tokio::test]
async fn intake_status_github_disabled_by_default() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["enabled"], false);
    Ok(())
}

#[tokio::test]
async fn intake_status_dashboard_always_enabled() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let dashboard = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "dashboard")
        .expect("dashboard channel present");
    assert_eq!(dashboard["enabled"], true);
    Ok(())
}

#[tokio::test]
async fn intake_status_shows_github_repo_when_configured() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "owner/myrepo".to_string(),
        label: "harness".to_string(),
        poll_interval_secs: 30,
        ..Default::default()
    });
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let github = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "github")
        .expect("github channel present");
    assert_eq!(github["enabled"], true);
    assert_eq!(github["repo"], "owner/myrepo");
    Ok(())
}

#[tokio::test]
async fn intake_status_recent_dispatches_empty_initially() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert!(json["recent_dispatches"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn intake_status_disables_feishu_when_verification_token_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), None).await?;
    let app = intake_app(state);

    use http_body_util::BodyExt;
    let response = app
        .oneshot(Request::builder().uri("/api/intake").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let feishu = json["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "feishu")
        .expect("feishu channel present");
    assert_eq!(feishu["enabled"], false);
    assert_eq!(feishu["keyword"], "harness");
    Ok(())
}

/// Build a minimal router that includes the auth middleware, mirroring how the
/// real server wires up the dashboard and tasks endpoints.
fn authed_app(state: Arc<AppState>) -> Router {
    use axum::middleware;
    Router::new()
        .route("/", get(crate::dashboard::index))
        .route("/health", get(health_check))
        .route("/tasks", get(list_tasks))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state)
}

/// / is now exempt from auth — dashboard HTML embeds no secrets.
#[tokio::test]
async fn dashboard_exempt_from_auth_when_token_configured() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = authed_app(state);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;

    // Dashboard is now exempt from auth — HTML contains no secrets.
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

/// Verify that query-param token no longer grants access to protected endpoints.
#[tokio::test]
async fn query_param_token_rejected_on_protected_endpoint() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("secret123".to_string());
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = authed_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/tasks?token=secret123")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn dashboard_no_auth_configured_remains_public() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = authed_app(state);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn list_tasks_exposes_task_kind_and_non_implementation_statuses() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .with_state(state.clone());

    let review_task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Review,
        status: task_runner::TaskStatus::ReviewWaiting,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("periodic_review".to_string()),
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: None,
        repo: Some("owner/repo".to_string()),
        description: Some("periodic review".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Review,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    let planner_task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Planner,
        status: task_runner::TaskStatus::PlannerGenerating,
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("sprint_planner".to_string()),
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: None,
        repo: Some("owner/repo".to_string()),
        description: Some("sprint planner".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Plan,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    state.core.tasks.insert(&review_task).await;
    state.core.tasks.insert(&planner_task).await;

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let tasks: serde_json::Value = serde_json::from_slice(&body)?;
    let tasks = tasks.as_array().expect("tasks array");
    assert!(tasks
        .iter()
        .any(|task| { task["task_kind"] == "review" && task["status"] == "review_waiting" }));
    assert!(tasks
        .iter()
        .any(|task| { task["task_kind"] == "planner" && task["status"] == "planner_generating" }));
    Ok(())
}

#[tokio::test]
async fn list_tasks_enriches_workflows_for_issue_and_pr_tasks() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = crate::test_helpers::tempdir_in_home("harness-test-task-workflows-")?;
    let mut state = make_test_state(dir.path()).await?;
    let workflow_store = Arc::new(
        harness_workflow::issue_lifecycle::IssueWorkflowStore::open(
            &harness_core::config::dirs::default_db_path(dir.path(), "issue_workflows"),
        )
        .await?,
    );
    let project_id = dir.path().display().to_string();

    workflow_store
        .record_issue_scheduled(
            project_id.as_str(),
            Some("owner/repo"),
            42,
            "task-issue",
            &[],
            false,
        )
        .await?;
    workflow_store
        .record_issue_scheduled(
            project_id.as_str(),
            Some("owner/repo"),
            77,
            "task-pr",
            &[],
            false,
        )
        .await?;
    workflow_store
        .record_pr_detected(
            project_id.as_str(),
            Some("owner/repo"),
            77,
            "task-pr",
            101,
            "https://github.com/owner/repo/pull/101",
        )
        .await?;
    workflow_store
        .record_issue_scheduled(
            "/tmp/other-project",
            Some("owner/repo"),
            42,
            "other-task",
            &[],
            false,
        )
        .await?;

    Arc::get_mut(&mut state)
        .expect("state should be uniquely owned")
        .core
        .issue_workflow_store = Some(workflow_store);

    let issue_task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:42".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        description: Some("issue task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    let pr_task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: Some("https://github.com/owner/repo/pull/101".to_string()),
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: Some(77),
        repo: Some("owner/repo".to_string()),
        description: Some("pr task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    let issue_task_id = issue_task.id.0.clone();
    let pr_task_id = pr_task.id.0.clone();
    state.core.tasks.insert(&issue_task).await;
    state.core.tasks.insert(&pr_task).await;

    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .with_state(state);

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let tasks: serde_json::Value = serde_json::from_slice(&body)?;
    let tasks = tasks.as_array().expect("tasks array");

    let issue_json = tasks
        .iter()
        .find(|task| task["id"] == issue_task_id)
        .expect("issue task should be listed");
    assert_eq!(issue_json["workflow"]["project_id"], project_id);
    assert_eq!(issue_json["workflow"]["issue_number"], 42);
    assert_eq!(issue_json["workflow"]["pr_number"], serde_json::Value::Null);

    let pr_json = tasks
        .iter()
        .find(|task| task["id"] == pr_task_id)
        .expect("pr task should be listed");
    assert_eq!(pr_json["workflow"]["project_id"], project_id);
    assert_eq!(pr_json["workflow"]["issue_number"], 77);
    assert_eq!(pr_json["workflow"]["pr_number"], 101);

    Ok(())
}

#[tokio::test]
async fn list_tasks_exposes_workflow_fallback_metadata() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_issue_workflows(dir.path()).await?;
    let app = Router::new()
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .with_state(state.clone());

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Done,
        turn: 3,
        pr_url: Some("https://github.com/owner/repo/pull/501".to_string()),
        rounds: vec![],
        error: Some("Review fallback tier C via silence".to_string()),
        source: Some("github".to_string()),
        external_id: Some("issue:945".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        issue: Some(945),
        repo: Some("owner/repo".to_string()),
        description: Some("issue #945".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Terminal,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    state.core.tasks.insert(&task).await;
    let workflows = state
        .core
        .issue_workflow_store
        .as_ref()
        .expect("workflow store");
    workflows
        .record_issue_scheduled(
            &dir.path().to_string_lossy(),
            Some("owner/repo"),
            945,
            task.id.as_str(),
            &[],
            false,
        )
        .await?;
    workflows
        .record_pr_detected(
            &dir.path().to_string_lossy(),
            Some("owner/repo"),
            945,
            task.id.as_str(),
            501,
            "https://github.com/owner/repo/pull/501",
        )
        .await?;
    workflows
        .record_ready_to_merge_with_fallback(
            &dir.path().to_string_lossy(),
            Some("owner/repo"),
            501,
            Some("fallback via silence"),
            harness_workflow::issue_lifecycle::ReviewFallbackSnapshot {
                tier: harness_workflow::issue_lifecycle::ReviewFallbackTier::C,
                trigger: harness_workflow::issue_lifecycle::ReviewFallbackTrigger::Silence,
                active_bot: Some("codex".to_string()),
                activated_at: Utc::now(),
            },
        )
        .await?;

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let tasks: serde_json::Value = serde_json::from_slice(&body)?;
    let task = tasks
        .as_array()
        .and_then(|tasks| tasks.first())
        .expect("task row");
    assert_eq!(task["workflow"]["state"], "ready_to_merge");
    assert_eq!(task["workflow"]["review_fallback"]["tier"], "c");
    assert_eq!(task["workflow"]["review_fallback"]["trigger"], "silence");
    assert_eq!(task["workflow"]["review_fallback"]["active_bot"], "codex");

    let detail_response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task["id"].as_str().expect("task id")))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(detail_response.status(), StatusCode::OK);
    let detail_body = axum::body::to_bytes(detail_response.into_body(), usize::MAX).await?;
    let detail: serde_json::Value = serde_json::from_slice(&detail_body)?;
    assert_eq!(detail["workflow"]["state"], "ready_to_merge");
    assert_eq!(detail["workflow"]["review_fallback"]["tier"], "c");
    assert!(detail["workflow"].get("events").is_none());
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_returns_service_unavailable_when_token_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), None).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(feishu_challenge_payload(None).to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = response_json(response).await?;
    assert_eq!(json["error"], "Feishu intake not configured");
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_returns_service_unavailable_when_token_is_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), Some("")).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(
                    feishu_challenge_payload(Some("secret-123")).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = response_json(response).await?;
    assert_eq!(json["error"], "Feishu intake not configured");
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_accepts_challenge_with_valid_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), Some("secret-123")).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(
                    feishu_challenge_payload(Some("secret-123")).to_string(),
                ))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    assert_eq!(json["challenge"], "challenge-123");
    Ok(())
}

#[tokio::test]
async fn feishu_webhook_rejects_invalid_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_feishu(dir.path(), Some("secret-123")).await?;
    let app = webhook_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook/feishu")
                .header("content-type", "application/json")
                .body(Body::from(feishu_event_payload(Some("wrong")).to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = response_json(response).await?;
    assert_eq!(json["error"], "invalid verification token");
    Ok(())
}

#[tokio::test]
async fn webhook_issues_opened_with_mention_schedules_runtime_issue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "org/repo".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/repo" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_issue_submission(
        &state,
        dir.path(),
        Some("org/repo"),
        77,
        json["task_id"].as_str().expect("task id should be present"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn webhook_issues_opened_requires_workflow_runtime_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    init_fake_git_repo(dir.path())?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "org/repo".to_string(),
        ..Default::default()
    });
    let (state, _agent) =
        make_test_state_with_agent_and_config(dir.path(), dir.path(), config).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/repo" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = response_json(response).await?;
    assert!(
        json["error"].as_str().is_some_and(|error| error
            .contains("workflow runtime store is required for GitHub issue submissions")),
        "unexpected response: {json}"
    );
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

#[tokio::test]
async fn webhook_routes_runtime_issue_to_repo_specific_project_root() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let repo_a_dir = crate::test_helpers::tempdir_in_home("webhook-repo-a-")?;
    let repo_b_dir = crate::test_helpers::tempdir_in_home("webhook-repo-b-")?;
    let secret = "secret";

    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "org/repo-b".to_string(),
            label: "harness".to_string(),
            project_root: Some(repo_b_dir.path().display().to_string()),
        }],
        ..Default::default()
    });

    let state = make_test_state_with_workflow_runtime_config_and_registry(
        repo_a_dir.path(),
        repo_a_dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/repo-b" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    assert_runtime_issue_submission(
        &state,
        repo_b_dir.path(),
        Some("org/repo-b"),
        77,
        json["task_id"].as_str().expect("task id should be present"),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn webhook_routes_runtime_prompt_to_repo_specific_project_root() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let repo_a_dir = crate::test_helpers::tempdir_in_home("webhook-prompt-a-")?;
    let repo_b_dir = crate::test_helpers::tempdir_in_home("webhook-prompt-b-")?;
    let secret = "secret";

    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "org/repo-b".to_string(),
            label: "harness".to_string(),
            project_root: Some(repo_b_dir.path().display().to_string()),
        }],
        ..Default::default()
    });

    let state = make_test_state_with_workflow_runtime_config_and_registry(
        repo_a_dir.path(),
        repo_a_dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "created",
        "issue": {
            "number": 42,
            "html_url": "https://github.com/org/repo-b/pull/42",
            "pull_request": { "url": "https://api.github.com/repos/org/repo-b/pulls/42" }
        },
        "comment": {
            "body": "@harness fix ci",
            "html_url": "https://github.com/org/repo-b/issues/42#issuecomment-1"
        },
        "repository": { "full_name": "org/repo-b" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    let runtime_task_id = json["task_id"].as_str().expect("task id should be present");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_prompt_submission(&state, repo_b_dir.path(), runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_ignores_issue_tasks_when_repo_is_unmapped() -> anyhow::Result<()> {
    let repo_a_dir = crate::test_helpers::tempdir_in_home("webhook-fallback-a-")?;
    let repo_b_dir = crate::test_helpers::tempdir_in_home("webhook-fallback-b-")?;
    let secret = "secret";

    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        repos: vec![harness_core::config::intake::GitHubRepoConfig {
            repo: "org/repo-b".to_string(),
            label: "harness".to_string(),
            project_root: Some(repo_b_dir.path().display().to_string()),
        }],
        ..Default::default()
    });

    let (state, _agent) =
        make_test_state_with_agent_and_config(repo_a_dir.path(), repo_a_dir.path(), config).await?;
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 78,
            "body": "@harness please implement this feature"
        },
        "repository": { "full_name": "org/unmapped" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issues")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "ignored");
    assert!(
        json["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not configured"),
        "reason should explain why the repo was ignored"
    );
    assert_eq!(state.core.tasks.count(), 0);
    Ok(())
}

#[tokio::test]
async fn webhook_pull_request_review_changes_requested_creates_runtime_prompt_submission(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = Some(secret.to_string());
    config.intake.github = Some(harness_core::config::intake::GitHubIntakeConfig {
        enabled: true,
        repo: "org/repo".to_string(),
        ..Default::default()
    });
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "submitted",
        "review": {
            "state": "changes_requested",
            "body": "Please fix the error handling.",
            "html_url": "https://github.com/org/repo/pull/10#pullrequestreview-1"
        },
        "pull_request": {
            "number": 10,
            "html_url": "https://github.com/org/repo/pull/10"
        },
        "repository": { "full_name": "org/repo" }
    });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "pull_request_review")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let json = response_json(response).await?;
    assert_eq!(json["status"], "implementing");
    assert_eq!(json["execution_path"], "workflow_runtime");
    let runtime_task_id = json["task_id"].as_str().expect("task id should be present");
    assert_eq!(state.core.tasks.count(), before_count);
    assert_runtime_prompt_submission(&state, dir.path(), runtime_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn webhook_ping_event_returns_accepted_without_creating_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({ "zen": "Design for failure." });
    let payload_body = payload.to_string();
    let signature = webhook_signature(secret, payload_body.as_bytes());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "ping")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(payload_body))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(state.core.tasks.count(), before_count);
    Ok(())
}

// --- Real router (build_router) coverage ---
// The tests above use hand-built minimal routers. The three tests below use
// the real `http_router::build_router` so that dropped routes, missing
// DefaultBodyLimit wiring, or removed auth middleware fail CI rather than
// failing only after deploy.

#[tokio::test]
async fn build_router_health_route_returns_ok() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = super::http_router::build_router(state);

    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn build_router_auth_middleware_blocks_unauthenticated_requests() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("test-token-abc".to_string());
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = super::http_router::build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/tasks")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    Ok(())
}

#[tokio::test]
async fn build_router_webhook_body_limit_enforced() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let body_limit = state.core.server.config.server.max_webhook_body_bytes;
    let app = super::http_router::build_router(state);

    let oversized = vec![b'a'; body_limit + 1024];
    let signature = webhook_signature(secret, &oversized);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhook")
                .header("x-github-event", "issue_comment")
                .header("x-hub-signature-256", signature)
                .header("content-type", "application/json")
                .body(Body::from(oversized))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

// --- Startup recovery coverage ---
// spawn_pr_recovery and spawn_checkpoint_recovery only run at server restart,
// so CI never exercised them before these tests. A regression in the callback,
// permit, or project-resolution wiring would leave tasks stuck in pending
// forever in production while CI stayed green.

async fn wait_for_task_status(
    state: &Arc<AppState>,
    task_id: &task_runner::TaskId,
    expected: task_runner::TaskStatus,
) -> anyhow::Result<task_runner::TaskState> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = state.core.tasks.get_with_db_fallback(task_id).await? {
            if task.status == expected {
                return Ok(task);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "task {} did not reach status {:?} within 5 seconds",
                task_id.0,
                expected
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn wait_for_task_to_leave_pending(
    state: &Arc<AppState>,
    task_id: &task_runner::TaskId,
) -> anyhow::Result<task_runner::TaskState> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(task) = state.core.tasks.get_with_db_fallback(task_id).await? {
            if task.status != task_runner::TaskStatus::Pending {
                return Ok(task);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("task {} did not leave Pending within 5 seconds", task_id.0);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn orphan_issue_task_is_redispatched_using_existing_task_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_pr_task_is_redispatched_using_existing_task_row() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            pr: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("pr:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_issue_task_is_redispatched_when_external_id_is_noncanonical() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("legacy-issue-944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_pr_task_is_redispatched_when_external_id_is_noncanonical() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            pr: Some(944),
            ..task_runner::CreateTaskRequest::default()
        });

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("legacy-pr-944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state = wait_for_task_to_leave_pending(&state, &task_id).await?;
    assert_eq!(final_state.id, task_id);
    assert_ne!(final_state.status, task_runner::TaskStatus::Pending);
    assert_eq!(state.core.tasks.list_all_with_terminal().await?.len(), 1);
    assert!(agent.prompts.lock().await.len() <= 1);
    Ok(())
}

#[tokio::test]
async fn orphan_issue_task_with_prompt_context_fails_when_identifier_is_missing(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let mut settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            prompt: Some("extra issue context".to_string()),
            ..task_runner::CreateTaskRequest::default()
        });
    settings.issue = None;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("legacy-issue-without-number".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: Some(settings),
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state =
        wait_for_task_status(&state, &task_id, task_runner::TaskStatus::Failed).await?;
    assert_eq!(
        final_state.error.as_deref(),
        Some("orphaned issue task: issue number not persisted")
    );
    assert!(agent.prompts.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn orphan_prompt_only_task_fails_closed_when_prompt_was_not_persisted() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Prompt,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: Some("prompt task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state =
        wait_for_task_status(&state, &task_id, task_runner::TaskStatus::Failed).await?;
    assert_eq!(
        final_state.error.as_deref(),
        Some("orphaned prompt-only task: prompt not persisted")
    );
    assert!(agent.prompts.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn orphan_prompt_only_task_waits_for_runtime_host_lease_to_expire_before_failing(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let mut task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Prompt,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: Some("prompt task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::milliseconds(75),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_state =
        wait_for_task_status(&state, &task_id, task_runner::TaskStatus::Failed).await?;
    assert_eq!(
        final_state.error.as_deref(),
        Some("orphaned prompt-only task: prompt not persisted")
    );
    assert_eq!(final_state.scheduler.runtime_host_id(), None);
    assert!(matches!(
        final_state.scheduler.authority_state,
        task_runner::SchedulerAuthorityState::Failed
    ));
    assert!(agent.prompts.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn orphan_recovery_excludes_pr_checkpoint_and_non_pending_rows() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (state, agent) = make_test_state_with_agent(dir.path(), Some("secret")).await?;

    let orphan = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: Some(944),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("issue #944".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let orphan_id = orphan.id.clone();
    state.core.tasks.insert(&orphan).await;

    let with_pr = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: Some("https://github.com/majiayu000/harness/pull/944".to_string()),
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("pr:944".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: Some("majiayu000/harness".to_string()),
        description: Some("PR #944".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let with_pr_id = with_pr.id.clone();
    state.core.tasks.insert(&with_pr).await;

    let with_checkpoint = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: Some("github".to_string()),
        external_id: Some("issue:945".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: Some(945),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("issue #945".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let with_checkpoint_id = with_checkpoint.id.clone();
    state.core.tasks.insert(&with_checkpoint).await;
    state
        .core
        .tasks
        .write_checkpoint(&with_checkpoint_id, None, Some("plan output"), None, "plan")
        .await?;

    let failed = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Issue,
        status: task_runner::TaskStatus::Failed,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: Some("already failed".to_string()),
        source: Some("github".to_string()),
        external_id: Some("issue:946".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: Some(946),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("issue #946".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let failed_id = failed.id.clone();
    state.core.tasks.insert(&failed).await;

    super::background::spawn_orphan_pending_recovery(&state).await;

    let final_orphan = wait_for_task_to_leave_pending(&state, &orphan_id).await?;
    assert_eq!(final_orphan.id, orphan_id);
    assert_ne!(final_orphan.status, task_runner::TaskStatus::Pending);
    assert!(agent.prompts.lock().await.len() <= 1);

    let pr_state = state
        .core
        .tasks
        .get_with_db_fallback(&with_pr_id)
        .await?
        .expect("pr task should exist");
    assert_eq!(pr_state.status, task_runner::TaskStatus::Pending);

    let checkpoint_state = state
        .core
        .tasks
        .get_with_db_fallback(&with_checkpoint_id)
        .await?
        .expect("checkpoint task should exist");
    assert_eq!(checkpoint_state.status, task_runner::TaskStatus::Pending);

    let failed_state = state
        .core
        .tasks
        .get_with_db_fallback(&failed_id)
        .await?
        .expect("failed task should exist");
    assert_eq!(failed_state.status, task_runner::TaskStatus::Failed);
    Ok(())
}

#[tokio::test]
async fn pr_recovery_marks_task_failed_when_pr_url_unparseable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Pr,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: Some("not-a-valid-pr-url".to_string()),
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_pr_recovery(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("task was not marked Failed within 5 seconds after pr_recovery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert!(matches!(
        final_state.status,
        task_runner::TaskStatus::Failed
    ));
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("unparseable pr_url"),
        "error should mention unparseable pr_url, got: {:?}",
        final_state.error
    );
    Ok(())
}

#[tokio::test]
async fn pr_recovery_redispatches_prompt_tasks_with_pr_urls() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Prompt,
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: Some("not-a-valid-pr-url".to_string()),
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_pr_recovery(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("prompt task was not re-dispatched within 5 seconds after pr_recovery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert!(matches!(
        final_state.status,
        task_runner::TaskStatus::Failed
    ));
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("unparseable pr_url"),
        "error should mention unparseable pr_url, got: {:?}",
        final_state.error
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_recovery_marks_prompt_task_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::Prompt,
        status: task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        repo: None,
        description: Some("prompt task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;
    // Write a checkpoint so the task appears in pending_tasks_with_checkpoint().
    state
        .core
        .tasks
        .write_checkpoint(&task_id, None, Some("plan output"), None, "plan")
        .await?;

    super::background::spawn_checkpoint_recovery(&state).await;

    // The spawned tokio task updates the cache; give it a moment to complete.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("prompt task was not marked Failed within 5 seconds after checkpoint_recovery");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert!(matches!(
        final_state.status,
        task_runner::TaskStatus::Failed
    ));
    assert!(
        final_state
            .error
            .as_deref()
            .unwrap_or("")
            .contains("prompt task"),
        "error should mention prompt task recovery failure, got: {:?}",
        final_state.error
    );
    Ok(())
}

#[test]
fn recovery_queue_domain_routes_review_tasks_to_review_capacity() {
    assert_eq!(
        super::background::recovery_queue_domain(task_runner::TaskKind::Review),
        super::task_routes::QueueDomain::Review
    );
    assert_eq!(
        super::background::recovery_queue_domain(task_runner::TaskKind::Planner),
        super::task_routes::QueueDomain::Primary
    );
}

#[tokio::test]
async fn get_task_exposes_workspace_lifecycle_metadata() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = crate::test_helpers::tempdir_in_home("harness-test-task-metadata-")?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        status: task_runner::TaskStatus::Failed,
        failure_kind: Some(task_runner::TaskFailureKind::WorkspaceLifecycle),
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: Some("workspace lifecycle reconciliation failed".to_string()),
        source: Some("github".to_string()),
        external_id: Some("issue:899".to_string()),
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: Some(dir.path().to_path_buf()),
        workspace_path: Some(dir.path().join("workspaces/task-899")),
        workspace_owner: Some("session-899".to_string()),
        run_generation: 3,
        issue: Some(899),
        repo: Some("majiayu000/harness".to_string()),
        description: Some("workspace failure".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::Implement,
        triage_output: None,
        plan_output: None,
        request_settings: None,
        task_kind: task_runner::TaskKind::Issue,
        scheduler: task_runner::TaskSchedulerState::queued(),
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    let app = task_app(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{}", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let json = response_json(response).await?;
    assert_eq!(json["failure_kind"], "workspace_lifecycle");
    assert_eq!(json["workspace_owner"], "session-899");
    assert_eq!(json["run_generation"], 3);
    assert!(json["workspace_path"]
        .as_str()
        .unwrap_or("")
        .ends_with("workspaces/task-899"));
    Ok(())
}

#[tokio::test]
async fn pr_recovery_waits_for_runtime_host_lease_to_expire() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let mut task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::default(),
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: Some("not-a-valid-pr-url".to_string()),
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::milliseconds(75),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::background::spawn_pr_recovery(&state);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("task was not recovered within 5 seconds after lease expiry");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert_eq!(final_state.scheduler.runtime_host_id(), None);
    assert!(matches!(
        final_state.scheduler.authority_state,
        task_runner::SchedulerAuthorityState::Failed
    ));
    Ok(())
}

#[tokio::test]
async fn checkpoint_recovery_waits_for_runtime_host_lease_to_expire() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let mut task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
        task_kind: task_runner::TaskKind::default(),
        status: task_runner::TaskStatus::Pending,
        turn: 0,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        issue: None,
        repo: None,
        description: Some("prompt task".to_string()),
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
        scheduler: task_runner::TaskSchedulerState::queued(),
        failure_kind: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
    };
    task.scheduler.claim_runtime_host(
        "host-a",
        chrono::Utc::now() + chrono::TimeDelta::milliseconds(75),
    );
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;
    state
        .core
        .tasks
        .write_checkpoint(&task_id, None, Some("plan output"), None, "plan")
        .await?;

    super::background::spawn_checkpoint_recovery(&state).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(t) = state.core.tasks.get(&task_id) {
            if matches!(t.status, task_runner::TaskStatus::Failed) {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("checkpoint task was not recovered within 5 seconds after lease expiry");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let final_state = state
        .core
        .tasks
        .get(&task_id)
        .expect("task must still exist");
    assert_eq!(final_state.scheduler.runtime_host_id(), None);
    assert!(matches!(
        final_state.scheduler.authority_state,
        task_runner::SchedulerAuthorityState::Failed
    ));
    Ok(())
}
