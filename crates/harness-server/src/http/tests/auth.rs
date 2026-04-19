use super::super::*;
use super::common::*;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{middleware, routing::get, Router};
use harness_core::{
    agent::AgentRequest, agent::AgentResponse, agent::CodeAgent, agent::StreamItem,
    types::Capability, types::TokenUsage,
};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

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

fn authed_app(state: Arc<AppState>) -> Router {
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

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

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

// --- Real router (build_router) coverage ---
// These tests use the real http_router::build_router so that dropped routes,
// missing DefaultBodyLimit wiring, or removed auth middleware fail CI rather
// than failing only after deploy.

#[tokio::test]
async fn build_router_health_route_returns_ok() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = super::super::http_router::build_router(state);

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
    let app = super::super::http_router::build_router(state);

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
async fn build_router_api_tasks_route_is_registered() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;
    let app = super::super::http_router::build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/tasks")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn build_router_api_tasks_auth_requires_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.api_token = Some("test-token-abc".to_string());
    let state = make_test_state_with(
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let app = super::super::http_router::build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/tasks")
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
    let app = super::super::http_router::build_router(state);

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

#[tokio::test]
async fn pr_recovery_marks_task_failed_when_pr_url_unparseable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
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
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;

    super::super::background::spawn_pr_recovery(&state);

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
async fn checkpoint_recovery_marks_prompt_task_failed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let state = make_test_state(dir.path()).await?;

    let task = task_runner::TaskState {
        id: task_runner::TaskId::new(),
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
        priority: 0,
        phase: task_runner::TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        request_settings: None,
    };
    let task_id = task.id.clone();
    state.core.tasks.insert(&task).await;
    // Write a triage checkpoint so pending_tasks_with_checkpoint (which JOINs
    // task_checkpoints) returns this task during recovery.
    state
        .core
        .tasks
        .write_checkpoint(&task_id, Some("triage done"), None, None, "triage")
        .await?;

    super::super::background::spawn_checkpoint_recovery(&state).await;

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
