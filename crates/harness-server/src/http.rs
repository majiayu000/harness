use crate::{router, server::HarnessServer, task_runner};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use harness_protocol::{RpcNotification, RpcRequest};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

pub struct AppState {
    pub server: Arc<HarnessServer>,
    pub project_root: std::path::PathBuf,
    pub tasks: Arc<task_runner::TaskStore>,
    pub skills: Arc<RwLock<harness_skills::SkillStore>>,
    pub rules: Arc<RwLock<harness_rules::engine::RuleEngine>>,
    pub events: Arc<harness_observe::EventStore>,
    pub gc_agent: Arc<harness_gc::GcAgent>,
    pub plans:
        Arc<RwLock<std::collections::HashMap<harness_core::ExecPlanId, harness_exec::ExecPlan>>>,
    pub plan_db: Option<crate::plan_db::PlanDb>,
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
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
}

impl AppState {
    pub fn observe_notification_lag(&self, dropped: u64) -> u64 {
        let previous_total = self
            .notification_lagged_total
            .fetch_add(dropped, Ordering::Relaxed);
        let dropped_total = previous_total.saturating_add(dropped);
        let log_every = self.notification_lag_log_every;
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
        session_renewal_secs = server.config.observe.session_renewal_secs,
        log_retention_days = server.config.observe.log_retention_days,
        "harness: effective config"
    );

    let db_path = dir.join("tasks.db");
    tracing::info!("task db: {}", db_path.display());
    let tasks = task_runner::TaskStore::open(&db_path).await?;

    let mut rule_engine = harness_rules::engine::RuleEngine::new();
    rule_engine.configure_sources(
        server.config.rules.discovery_paths.clone(),
        server.config.rules.builtin_path.clone(),
    );
    if let Err(e) = rule_engine.load_builtin() {
        tracing::warn!("failed to load builtin rules: {e}");
    }

    let events = Arc::new(harness_observe::EventStore::with_policies(
        &dir,
        server.config.observe.session_renewal_secs,
        server.config.observe.log_retention_days,
    )?);

    let signal_detector = harness_gc::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::ProjectId::new(),
    );
    let draft_store = harness_gc::DraftStore::new(&dir)?;
    let gc_agent = Arc::new(harness_gc::GcAgent::new(
        harness_gc::gc_agent::GcConfig::default(),
        signal_detector,
        draft_store,
    ));

    let thread_db_path = dir.join("threads.db");
    let thread_db = crate::thread_db::ThreadDb::open(&thread_db_path).await?;
    let plan_db = crate::plan_db::PlanDb::open(&dir.join("plans.db")).await?;
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

    let mut skill_store = harness_skills::SkillStore::new().with_persist_dir(dir.join("skills"));
    skill_store.load_builtin();
    if let Err(e) = skill_store.discover() {
        tracing::warn!("Failed to reload persisted skills on startup: {}", e);
    }

    Ok(AppState {
        server,
        project_root,
        tasks,
        skills: Arc::new(RwLock::new(skill_store)),
        rules: Arc::new(RwLock::new(rule_engine)),
        events,
        gc_agent,
        plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
        thread_db: Some(thread_db),
        plan_db: Some(plan_db),
        interceptors: vec![Arc::new(crate::contract_validator::ContractValidator::new())],
        notification_tx: broadcast::channel(notification_broadcast_capacity).0,
        notification_lagged_total: Arc::new(AtomicU64::new(0)),
        notification_lag_log_every,
        notify_tx: None,
        initialized: Arc::new(AtomicBool::new(false)),
    })
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

#[derive(Debug)]
enum EnqueueTaskError {
    BadRequest(String),
    Internal(String),
}

async fn enqueue_task(
    state: &Arc<AppState>,
    req: task_runner::CreateTaskRequest,
) -> Result<task_runner::TaskId, EnqueueTaskError> {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return Err(EnqueueTaskError::BadRequest(
            "at least one of prompt, issue, or pr must be provided".to_string(),
        ));
    }

    let agent =
        if let Some(name) = &req.agent {
            state.server.agent_registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            })?
        } else {
            state
                .server
                .agent_registry
                .default_agent()
                .ok_or_else(|| EnqueueTaskError::Internal("no agent registered".to_string()))?
        };

    let (reviewer, review_config) = resolve_reviewer(
        &state.server.agent_registry,
        &state.server.config.agents.review,
        agent.name(),
    );

    let task_id = task_runner::spawn_task(
        state.tasks.clone(),
        agent,
        reviewer,
        review_config,
        state.skills.clone(),
        state.events.clone(),
        state.interceptors.clone(),
        req,
    )
    .await;

    Ok(task_id)
}

#[derive(Debug, Deserialize)]
struct GitHubRepositoryRef {
    full_name: String,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueRef {
    number: u64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GitHubCommentRef {
    body: String,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubIssueCommentEvent {
    action: String,
    issue: GitHubIssueRef,
    comment: GitHubCommentRef,
    repository: GitHubRepositoryRef,
}

#[derive(Debug, Deserialize)]
struct GitHubIssuesEvent {
    action: String,
    issue: GitHubIssueRef,
}

fn issue_task_request(issue_number: u64) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest::default();
    req.issue = Some(issue_number);
    req
}

fn review_task_request(pr_number: u64) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest::default();
    req.pr = Some(pr_number);
    req
}

fn fix_ci_task_request(payload: &GitHubIssueCommentEvent) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest::default();
    req.prompt = Some(crate::task_executor::build_fix_ci_prompt(
        &payload.repository.full_name,
        payload.issue.number,
        &payload.comment.body,
        payload.comment.html_url.as_deref(),
        payload.issue.html_url.as_deref(),
    ));
    req
}

fn parse_github_webhook_task_request(
    event: &str,
    payload: &[u8],
) -> Result<(Option<task_runner::CreateTaskRequest>, String), String> {
    match event {
        "ping" => Ok((None, "ping".to_string())),
        "issues" => {
            let parsed: GitHubIssuesEvent = serde_json::from_slice(payload)
                .map_err(|err| format!("invalid issues payload: {err}"))?;
            if !matches!(parsed.action.as_str(), "opened" | "edited" | "reopened") {
                return Ok((None, format!("ignored issues action '{}'", parsed.action)));
            }
            if parsed.issue.pull_request.is_some() {
                return Ok((None, "issues event references pull request".to_string()));
            }
            match crate::task_executor::parse_harness_mention_command(
                parsed.issue.body.as_deref().unwrap_or(""),
            ) {
                Some(crate::task_executor::HarnessMentionCommand::Mention) => Ok((
                    Some(issue_task_request(parsed.issue.number)),
                    "issue mention".to_string(),
                )),
                Some(crate::task_executor::HarnessMentionCommand::Review) => {
                    Ok((None, "review command ignored on issue body".to_string()))
                }
                Some(crate::task_executor::HarnessMentionCommand::FixCi) => {
                    Ok((None, "fix ci command ignored on issue body".to_string()))
                }
                None => Ok((None, "no @harness command in issue body".to_string())),
            }
        }
        "issue_comment" => {
            let parsed: GitHubIssueCommentEvent = serde_json::from_slice(payload)
                .map_err(|err| format!("invalid issue_comment payload: {err}"))?;
            if parsed.action != "created" {
                return Ok((
                    None,
                    format!("ignored issue_comment action '{}'", parsed.action),
                ));
            }

            let command =
                match crate::task_executor::parse_harness_mention_command(&parsed.comment.body) {
                    Some(command) => command,
                    None => return Ok((None, "no @harness command in comment".to_string())),
                };

            if parsed.issue.pull_request.is_some() {
                return match command {
                    crate::task_executor::HarnessMentionCommand::Mention
                    | crate::task_executor::HarnessMentionCommand::Review => Ok((
                        Some(review_task_request(parsed.issue.number)),
                        "pr review command".to_string(),
                    )),
                    crate::task_executor::HarnessMentionCommand::FixCi => Ok((
                        Some(fix_ci_task_request(&parsed)),
                        "pr fix ci command".to_string(),
                    )),
                };
            }

            match command {
                crate::task_executor::HarnessMentionCommand::Mention => Ok((
                    Some(issue_task_request(parsed.issue.number)),
                    "issue mention command".to_string(),
                )),
                crate::task_executor::HarnessMentionCommand::Review => {
                    Ok((None, "review command ignored on issue comment".to_string()))
                }
                crate::task_executor::HarnessMentionCommand::FixCi => {
                    Ok((None, "fix ci command ignored on issue comment".to_string()))
                }
            }
        }
        _ => Ok((None, format!("unsupported event '{event}'"))),
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for pair in bytes.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)? as u8;
        let low = (pair[1] as char).to_digit(16)? as u8;
        out.push((high << 4) | low);
    }
    Some(out)
}

fn verify_github_signature(secret: &str, signature_header: &str, payload: &[u8]) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let Some(signature_hex) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Some(signature_bytes) = decode_hex(signature_hex) else {
        return false;
    };

    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(payload);
    mac.verify_slice(&signature_bytes).is_ok()
}

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");

    let state = Arc::new(build_app_state(server).await?);

    let initial_grade = {
        let events = state
            .events
            .query(&harness_core::EventFilters::default())
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

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/rpc", post(handle_rpc))
        .route("/ws", get(crate::websocket::ws_handler))
        .route("/tasks", post(create_task))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .route("/webhook", post(github_webhook))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.tasks.count();
    Json(json!({"status": "ok", "tasks": count}))
}

async fn handle_rpc(State(state): State<Arc<AppState>>, Json(req): Json<RpcRequest>) -> Response {
    match router::handle_request(&state, req).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<task_runner::CreateTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match enqueue_task(&state, req).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "task_id": task_id.0,
                "status": "running"
            })),
        ),
        Err(EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
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

    if let Some(secret) = state
        .server
        .config
        .server
        .github_webhook_secret
        .as_deref()
        .filter(|secret| !secret.is_empty())
    {
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
        if !verify_github_signature(secret, signature, body.as_ref()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid webhook signature"})),
            );
        }
    }

    let (request, reason) = match parse_github_webhook_task_request(event, body.as_ref()) {
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
        req.project = Some(state.project_root.clone());
    }

    match enqueue_task(&state, req).await {
        Ok(task_id) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "accepted",
                "reason": reason,
                "task_id": task_id.0,
            })),
        ),
        Err(EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn list_tasks(State(state): State<Arc<AppState>>) -> Json<Vec<task_runner::TaskSummary>> {
    let tasks = state
        .tasks
        .list_all()
        .into_iter()
        .map(|t| t.summary())
        .collect();
    Json(tasks)
}

async fn get_task(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.tasks.get(&task_runner::TaskId(id)) {
        Some(task) => Json(task).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use harness_core::{
        AgentRequest, AgentResponse, Capability, CodeAgent, StreamItem, TokenUsage,
    };
    use std::sync::Arc;
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

        async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
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
        ) -> harness_core::Result<()> {
            Ok(())
        }
    }

    async fn make_test_state_with(
        dir: &std::path::Path,
        config: harness_core::HarnessConfig,
        agent_registry: harness_agents::AgentRegistry,
    ) -> anyhow::Result<Arc<AppState>> {
        let thread_manager = crate::thread_manager::ThreadManager::new();
        let server = Arc::new(crate::server::HarnessServer::new(
            config,
            thread_manager,
            agent_registry,
        ));
        let tasks = task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            server.config.gc.signal_thresholds.clone().into(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        let project_root = std::env::current_dir().unwrap_or_else(|_| dir.to_path_buf());
        Ok(Arc::new(AppState {
            server,
            project_root,
            tasks,
            skills: Arc::new(tokio::sync::RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(tokio::sync::RwLock::new(
                harness_rules::engine::RuleEngine::new(),
            )),
            events,
            gc_agent,
            plans: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            plan_db: None,
            interceptors: vec![],
            notification_tx: tokio::sync::broadcast::channel(32).0,
            notification_lagged_total: Arc::new(AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(AtomicBool::new(true)),
        }))
    }

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
        make_test_state_with(
            dir,
            harness_core::HarnessConfig::default(),
            harness_agents::AgentRegistry::new("test"),
        )
        .await
    }

    async fn make_test_state_with_agent(
        dir: &std::path::Path,
        webhook_secret: Option<&str>,
    ) -> anyhow::Result<(Arc<AppState>, Arc<CapturingAgent>)> {
        let mut config = harness_core::HarnessConfig::default();
        config.server.github_webhook_secret = webhook_secret.map(ToString::to_string);

        let capturing = CapturingAgent::new();
        let mut registry = harness_agents::AgentRegistry::new("test");
        registry.register("test", capturing.clone());

        let state = make_test_state_with(dir, config, registry).await?;
        Ok((state, capturing))
    }

    fn webhook_app(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/webhook", post(github_webhook))
            .with_state(state)
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
    async fn webhook_issue_mention_creates_issue_task() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (state, agent) = make_test_state_with_agent(dir.path(), None).await?;
        let before_count = state.tasks.count();
        let app = webhook_app(state.clone());

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

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(state.tasks.count(), before_count + 1);
        let _ = agent;
        Ok(())
    }

    #[tokio::test]
    async fn webhook_review_on_pr_creates_pr_review_task() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (state, agent) = make_test_state_with_agent(dir.path(), None).await?;
        let before_count = state.tasks.count();
        let app = webhook_app(state.clone());

        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 42, "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" } },
            "comment": { "body": "@harness review" },
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

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(state.tasks.count(), before_count + 1);
        let _ = agent;
        Ok(())
    }

    #[tokio::test]
    async fn webhook_fix_ci_on_pr_creates_fix_ci_task() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (state, agent) = make_test_state_with_agent(dir.path(), None).await?;
        let before_count = state.tasks.count();
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

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(state.tasks.count(), before_count + 1);
        let _ = agent;
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

    #[test]
    fn decode_hex_rejects_invalid_input() {
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
    }

    #[test]
    fn verify_github_signature_accepts_valid_hmac() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let secret = "top-secret";
        let payload = br#"{"ok":true}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let signature = mac.finalize().into_bytes();
        let signature_hex: String = signature.iter().map(|byte| format!("{byte:02x}")).collect();
        let header = format!("sha256={signature_hex}");

        assert!(verify_github_signature(secret, &header, payload));
    }

    #[test]
    fn parse_issue_comment_issue_mention_maps_to_issue_request() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 106 },
            "comment": { "body": "@harness please handle this issue" },
            "repository": { "full_name": "majiayu000/harness" }
        });

        let (request, _) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(request.issue, Some(106));
        assert_eq!(request.pr, None);
        assert_eq!(request.prompt, None);
    }

    #[test]
    fn parse_issue_comment_pr_review_maps_to_pr_request() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 42, "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" } },
            "comment": { "body": "@harness review" },
            "repository": { "full_name": "majiayu000/harness" }
        });

        let (request, _) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(request.issue, None);
        assert_eq!(request.pr, Some(42));
        assert_eq!(request.prompt, None);
    }

    #[test]
    fn parse_issue_comment_fix_ci_maps_to_prompt_request() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": {
                "number": 42,
                "html_url": "https://github.com/majiayu000/harness/pull/42",
                "pull_request": { "url": "https://api.github.com/repos/majiayu000/harness/pulls/42" }
            },
            "comment": {
                "body": "@harness fix ci",
                "html_url": "https://github.com/majiayu000/harness/issues/42#issuecomment-1"
            },
            "repository": { "full_name": "majiayu000/harness" }
        });

        let (request, _) =
            parse_github_webhook_task_request("issue_comment", payload.to_string().as_bytes())
                .unwrap();
        let request = request.expect("request should exist");
        assert_eq!(request.issue, None);
        assert_eq!(request.pr, None);
        assert!(request
            .prompt
            .as_deref()
            .unwrap_or_default()
            .contains("CI failure repair requested for PR #42"));
    }
}
