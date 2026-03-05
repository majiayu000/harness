use crate::{router, server::HarnessServer, task_runner};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use harness_protocol::{RpcNotification, RpcRequest, RpcResponse};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

pub struct AppState {
    pub server: Arc<HarnessServer>,
    pub tasks: Arc<task_runner::TaskStore>,
    pub skills: Arc<RwLock<harness_skills::SkillStore>>,
    pub rules: Arc<RwLock<harness_rules::engine::RuleEngine>>,
    pub events: Arc<harness_observe::EventStore>,
    pub gc_agent: Arc<harness_gc::GcAgent>,
    pub plans: Arc<RwLock<std::collections::HashMap<harness_core::ExecPlanId, harness_exec::ExecPlan>>>,
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    /// Broadcast channel for server-push notifications (WebSocket and stdio transports).
    pub notification_tx: broadcast::Sender<RpcNotification>,
}

fn data_dir() -> std::path::PathBuf {
    let db_path = std::env::var("HARNESS_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("harness")
        });
    if db_path.extension().is_some() {
        db_path.parent().unwrap_or(std::path::Path::new(".")).to_path_buf()
    } else {
        db_path
    }
}

/// Build an AppState with all stores. Used by both HTTP and stdio transports.
pub async fn build_app_state(server: Arc<HarnessServer>) -> anyhow::Result<AppState> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;

    let db_path = dir.join("tasks.db");
    tracing::info!("task db: {}", db_path.display());
    let tasks = task_runner::TaskStore::open(&db_path).await?;

    let mut rule_engine = harness_rules::engine::RuleEngine::new();
    if let Err(e) = rule_engine.load_builtin() {
        tracing::warn!("failed to load builtin rules: {e}");
    }

    let events = Arc::new(harness_observe::EventStore::new(&dir)?);

    let signal_detector = harness_gc::SignalDetector::new(
        harness_gc::signal_detector::SignalThresholds::default(),
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
    // Load persisted threads into the in-memory ThreadManager cache
    for thread in thread_db.list().await? {
        server.thread_manager.threads_cache().insert(
            thread.id.as_str().to_string(),
            thread,
        );
    }

    Ok(AppState {
        server,
        tasks,
        skills: Arc::new(RwLock::new({
            let mut store = harness_skills::SkillStore::new().with_persist_dir(dir.join("skills"));
            store.load_builtin();
            store
        })),
        rules: Arc::new(RwLock::new(rule_engine)),
        events,
        gc_agent,
        plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
        thread_db: Some(thread_db),
        interceptors: vec![Arc::new(crate::contract_validator::ContractValidator::new())],
        notification_tx: broadcast::channel(256).0,
    })
}

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");

    let state = Arc::new(build_app_state(server).await?);

    let initial_grade = {
        let events = state.events.query(&harness_core::EventFilters::default()).unwrap_or_default();
        let violation_count = events
            .iter()
            .filter(|e| e.hook == "rule_check")
            .count();
        harness_observe::quality::QualityGrader::grade(&events, violation_count).grade
    };
    crate::scheduler::Scheduler::from_grade(initial_grade).start(state.clone());

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/rpc", post(handle_rpc))
        .route("/tasks", post(create_task))
        .route("/tasks", get(list_tasks))
        .route("/tasks/{id}", get(get_task))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.tasks.count();
    Json(json!({"status": "ok", "tasks": count}))
}

async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RpcRequest>,
) -> Json<RpcResponse> {
    Json(router::handle_request(&state, req).await)
}

async fn create_task(
    State(state): State<Arc<AppState>>,
    Json(req): Json<task_runner::CreateTaskRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one of prompt, issue, or pr must be provided"})),
        );
    }

    let agent = match state.server.agent_registry.default_agent() {
        Some(a) => a,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "no agent registered"})),
            );
        }
    };

    let task_id = task_runner::spawn_task(state.tasks.clone(), agent, state.skills.clone(), state.events.clone(), state.interceptors.clone(), req).await;

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "task_id": task_id.0,
            "status": "running"
        })),
    )
}

async fn list_tasks(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<task_runner::TaskSummary>> {
    let tasks = state.tasks.list_all().into_iter().map(|t| t.summary()).collect();
    Json(tasks)
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
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
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<Arc<AppState>> {
        let config = harness_core::HarnessConfig::default();
        let thread_manager = crate::thread_manager::ThreadManager::new();
        let agent_registry = harness_agents::AgentRegistry::new("test");
        let server = Arc::new(crate::server::HarnessServer::new(config, thread_manager, agent_registry));
        let tasks = task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        Ok(Arc::new(AppState {
            server,
            tasks,
            skills: Arc::new(tokio::sync::RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(tokio::sync::RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            interceptors: vec![],
            notification_tx: tokio::sync::broadcast::channel(32).0,
        }))
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok_and_task_count() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let app = Router::new()
            .route("/health", get(health_check))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())?,
            )
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
}
