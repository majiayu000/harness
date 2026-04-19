use super::super::*;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, post},
    Router,
};
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, agent::StreamItem,
    types::Capability, types::TokenUsage,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

pub(crate) struct CapturingAgent {
    pub(crate) prompts: Mutex<Vec<String>>,
}

impl CapturingAgent {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            prompts: Mutex::new(Vec::new()),
        })
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
        Ok(AgentResponse {
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
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
}

pub(crate) async fn make_test_state_with(
    dir: &std::path::Path,
    config: harness_core::config::HarnessConfig,
    agent_registry: harness_agents::registry::AgentRegistry,
) -> anyhow::Result<Arc<AppState>> {
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
    let tasks =
        task_runner::TaskStore::open(&harness_core::config::dirs::default_db_path(dir, "tasks"))
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
        dir.to_path_buf(),
    ));
    let thread_db = crate::thread_db::ThreadDb::open(&harness_core::config::dirs::default_db_path(
        dir, "threads",
    ))
    .await?;
    let _project_svc_tmp = crate::project_registry::ProjectRegistry::open(
        &harness_core::config::dirs::default_db_path(dir, "projects"),
    )
    .await?;
    let project_svc =
        crate::services::project::DefaultProjectService::new(_project_svc_tmp, dir.to_path_buf());
    let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        Default::default(),
        events.clone(),
        vec![],
        None,
        Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
        None,
        None,
        vec![],
    );
    Ok(Arc::new(AppState {
        core: crate::http::CoreServices {
            server,
            project_root: dir.to_path_buf(),
            home_dir: std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| dir.to_path_buf()),
            tasks,
            thread_db: Some(thread_db),
            plan_db: None,
            plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
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
            task_queue: Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            workspace_mgr: None,
        },
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

pub(crate) async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
    make_test_state_with(
        dir,
        harness_core::config::HarnessConfig::default(),
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

pub(crate) async fn make_test_state_with_agent(
    dir: &std::path::Path,
    webhook_secret: Option<&str>,
) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.github_webhook_secret = webhook_secret.map(ToString::to_string);

    let capturing = CapturingAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("test");
    registry.register("test", capturing.clone());

    let state = make_test_state_with(dir, config, registry).await?;
    Ok((state, capturing))
}

pub(crate) fn task_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/tasks", post(task_routes::create_task))
        .route("/tasks/{id}", get(get_task))
        .with_state(state)
}

pub(crate) fn list_tasks_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/tasks", get(list_tasks))
        .route("/api/tasks", get(list_tasks))
        .with_state(state)
}

pub(crate) fn intake_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/intake", get(intake_status))
        .with_state(state)
}

pub(crate) fn token_usage_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/token-usage",
            get(crate::handlers::token_usage::token_usage),
        )
        .with_state(state)
}

pub(crate) fn webhook_app(state: Arc<AppState>) -> Router {
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

pub(crate) fn make_feishu_config(
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

pub(crate) async fn make_test_state_with_feishu(
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

pub(crate) fn feishu_challenge_payload(token: Option<&str>) -> serde_json::Value {
    match token {
        Some(token) => serde_json::json!({ "challenge": "challenge-123", "token": token }),
        None => serde_json::json!({ "challenge": "challenge-123" }),
    }
}

pub(crate) fn feishu_event_payload(token: Option<&str>) -> serde_json::Value {
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

pub(crate) async fn response_json(
    response: axum::response::Response,
) -> anyhow::Result<serde_json::Value> {
    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

pub(crate) fn webhook_signature(secret: &str, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("valid hmac key");
    mac.update(payload);
    let digest = mac.finalize().into_bytes();
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={digest_hex}")
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct HealthResponse {
    pub(crate) status: String,
    pub(crate) tasks: u64,
    pub(crate) persistence: PersistenceBlock,
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct PersistenceBlock {
    pub(crate) degraded_subsystems: Vec<String>,
    pub(crate) runtime_state_dirty: bool,
}

pub(crate) async fn call_health(state: Arc<AppState>) -> anyhow::Result<HealthResponse> {
    use http_body_util::BodyExt;
    let app = Router::new()
        .route("/health", get(health_check))
        .with_state(state);
    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}
