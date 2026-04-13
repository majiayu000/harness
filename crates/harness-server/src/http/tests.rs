use super::*;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, agent::StreamItem,
    types::Capability, types::TokenUsage,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
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

async fn make_test_state_with(
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
    make_test_state_with(
        dir,
        harness_core::config::HarnessConfig::default(),
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
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

    let capturing = CapturingAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("test");
    registry.register("test", capturing.clone());

    let state = make_test_state_with(dir, config, registry).await?;
    Ok((state, capturing))
}

fn task_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/tasks", post(task_routes::create_task))
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

fn webhook_signature(secret: &str, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("valid hmac key");
    mac.update(payload);
    let digest = mac.finalize().into_bytes();
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={digest_hex}")
}

#[tokio::test]
async fn health_endpoint_returns_ok_and_task_count() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let app = Router::new()
        .route("/health", get(health_check))
        .with_state(state);

    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty())?)
        .await?;

    assert_eq!(response.status(), StatusCode::OK);

    #[derive(serde::Deserialize, Debug)]
    struct HealthResponse {
        status: String,
        tasks: u64,
    }

    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    let health: HealthResponse = serde_json::from_slice(&body)?;

    assert_eq!(health.status, "ok");
    assert_eq!(health.tasks, 0);
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
async fn webhook_issue_mention_creates_issue_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
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
    assert_eq!(state.core.tasks.count(), before_count + 1);
    Ok(())
}

#[tokio::test]
async fn webhook_review_on_pr_creates_pr_review_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
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
    assert_eq!(state.core.tasks.count(), before_count + 1);
    Ok(())
}

#[tokio::test]
async fn webhook_fix_ci_on_pr_creates_fix_ci_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
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
    assert_eq!(state.core.tasks.count(), before_count + 1);
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
async fn create_task_with_prompt_returns_accepted() -> anyhow::Result<()> {
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

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    use http_body_util::BodyExt;
    let resp_body = response.into_body().collect().await?.to_bytes();
    let resp: serde_json::Value = serde_json::from_slice(&resp_body)?;
    assert!(resp["task_id"].is_string());
    assert_eq!(state.core.tasks.count(), before_count + 1);
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
    let dir = tempfile::tempdir()?;
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some("s")).await?;

    let create_body = serde_json::json!({ "prompt": "add tests" });
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

    use http_body_util::BodyExt;
    let create_body = create_resp.into_body().collect().await?.to_bytes();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body)?;
    let task_id = create_json["task_id"]
        .as_str()
        .expect("task_id should be string");

    // GET the task — agent executes instantly (mock), so status should be observable
    let get_resp = task_app(state)
        .oneshot(
            Request::builder()
                .uri(format!("/tasks/{task_id}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_resp.status(), StatusCode::OK);

    let get_body = get_resp.into_body().collect().await?.to_bytes();
    let task_json: serde_json::Value = serde_json::from_slice(&get_body)?;
    assert_eq!(task_json["id"], task_id);
    Ok(())
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

#[tokio::test]
async fn dashboard_is_public_when_token_configured() -> anyhow::Result<()> {
    // The dashboard HTML (/) contains no sensitive data and is exempt from
    // the auth middleware.  Auth is enforced at the WebSocket first-message
    // level instead.
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

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn dashboard_query_param_token_no_longer_grants_api_access() -> anyhow::Result<()> {
    // ?token= is no longer accepted on any path — query-param auth was removed
    // to prevent token leakage via browser history and referrer headers.
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

    // A protected endpoint must reject query-param tokens.
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
async fn webhook_issues_opened_with_mention_creates_issue_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
    let before_count = state.core.tasks.count();
    let app = webhook_app(state.clone());

    let payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 77,
            "body": "@harness please implement this feature"
        }
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
    assert_eq!(state.core.tasks.count(), before_count + 1);
    Ok(())
}

#[tokio::test]
async fn webhook_pull_request_review_changes_requested_creates_task() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let secret = "secret";
    let (state, _agent) = make_test_state_with_agent(dir.path(), Some(secret)).await?;
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
    assert_eq!(state.core.tasks.count(), before_count + 1);
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

/// Agent stub that records invocations via an atomic flag.
/// Records in both `execute` and `execute_stream` since the task executor
/// calls `execute_stream` (via `run_agent_streaming`).
struct DispatchCapturingAgent {
    agent_name: &'static str,
    was_invoked: Arc<std::sync::atomic::AtomicBool>,
}

impl DispatchCapturingAgent {
    fn new(name: &'static str) -> (Arc<Self>, Arc<std::sync::atomic::AtomicBool>) {
        let was_invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let agent = Arc::new(Self {
            agent_name: name,
            was_invoked: was_invoked.clone(),
        });
        (agent, was_invoked)
    }
}

#[async_trait]
impl CodeAgent for DispatchCapturingAgent {
    fn name(&self) -> &str {
        self.agent_name
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        self.was_invoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
            model: self.agent_name.to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        self.was_invoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Build a test AppState with a "test" (default) and a "claude" DispatchCapturingAgent.
/// Returns the state and atomic flags for each agent so tests can detect which was invoked.
async fn make_test_state_with_dispatch_agents(
    dir: &std::path::Path,
) -> anyhow::Result<(
    Arc<AppState>,
    Arc<std::sync::atomic::AtomicBool>,
    Arc<std::sync::atomic::AtomicBool>,
)> {
    let (default_agent, default_invoked) = DispatchCapturingAgent::new("test");
    let (claude_agent, claude_invoked) = DispatchCapturingAgent::new("claude");
    let mut registry = harness_agents::registry::AgentRegistry::new("test");
    registry.set_complexity_preferences(vec!["claude".to_string()]);
    registry.register("test", default_agent);
    registry.register("claude", claude_agent);
    let state = make_test_state_with(
        dir,
        harness_core::config::HarnessConfig::default(),
        registry,
    )
    .await?;
    Ok((state, default_invoked, claude_invoked))
}

/// Poll (with 5 s deadline) until `flag` is set; panic with `msg` on timeout.
async fn wait_for_invocation(flag: &std::sync::atomic::AtomicBool, msg: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{msg}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn dispatch_complex_prompt_selects_claude_agent() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("dispatch-complex-")?;
    let (state, default_invoked, claude_invoked) =
        make_test_state_with_dispatch_agents(dir.path()).await?;
    let app = task_app(state);

    // 6 distinct file references → Complex complexity → registry.dispatch() selects "claude"
    let body = serde_json::json!({
        "prompt": "Refactor src/a.rs src/b.rs src/c.rs src/d.rs src/e.rs src/f.rs",
        "project": dir.path(),
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

    wait_for_invocation(
        &claude_invoked,
        "claude agent was not invoked within 500ms for complex task",
    )
    .await;

    assert!(
        claude_invoked.load(std::sync::atomic::Ordering::SeqCst),
        "claude agent should have been invoked for complex task"
    );
    assert!(
        !default_invoked.load(std::sync::atomic::Ordering::SeqCst),
        "default agent should not have been invoked for complex task"
    );
    Ok(())
}

#[tokio::test]
async fn dispatch_simple_prompt_selects_default_agent() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("dispatch-simple-")?;
    let (state, default_invoked, claude_invoked) =
        make_test_state_with_dispatch_agents(dir.path()).await?;
    let app = task_app(state);

    // Simple prompt: no file references → Simple complexity → default "test" agent
    let body = serde_json::json!({
        "prompt": "Fix the login bug",
        "project": dir.path(),
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

    wait_for_invocation(
        &default_invoked,
        "default agent was not invoked within 500ms for simple task",
    )
    .await;

    assert!(
        default_invoked.load(std::sync::atomic::Ordering::SeqCst),
        "default agent should have been invoked for simple task"
    );
    assert!(
        !claude_invoked.load(std::sync::atomic::Ordering::SeqCst),
        "claude agent should not have been invoked for simple task"
    );
    Ok(())
}
