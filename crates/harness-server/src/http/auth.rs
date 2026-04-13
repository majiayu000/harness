use super::AppState;
use axum::{
    extract::State,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// Resolve the effective API token from server config or `HARNESS_API_TOKEN` env var.
///
/// Filters empty strings *before* the env-var fallback so that an explicit
/// `api_token = ""` in server.toml does not shadow `HARNESS_API_TOKEN`.
pub(crate) fn resolve_api_token(
    config: &harness_core::config::server::ServerConfig,
) -> Option<String> {
    config
        .api_token
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_owned())
        .or_else(|| {
            std::env::var("HARNESS_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
        })
}

/// Bearer token authentication middleware.
///
/// Exempts `/health`, `/webhook`, `/webhook/feishu`, `/signals`, `/favicon.ico`,
/// `/auth/reset-password`, `/` (dashboard HTML), and `/ws` (WebSocket upgrade).
/// The dashboard and WebSocket are public at the HTTP level; the WebSocket uses
/// a first-message token handshake (see `websocket.rs`) when auth is configured.
/// All other endpoints require an `Authorization: Bearer <token>` header when
/// `api_token` is configured. When no token is configured the middleware is a
/// no-op (backward compat).
pub(crate) async fn api_auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    // Exempt paths: static assets, public health probes, and paths with their
    // own auth mechanism (/ws uses first-message handshake; / has no sensitive
    // data and auth is handled by the WS layer).
    if matches!(
        path,
        "/health"
            | "/webhook"
            | "/webhook/feishu"
            | "/signals"
            | "/favicon.ico"
            | "/auth/reset-password"
            | "/"
            | "/ws"
    ) {
        return next.run(req).await;
    }

    let Some(expected) = resolve_api_token(&state.core.server.config.server) else {
        // No token configured — skip auth for backward compatibility.
        return next.run(req).await;
    };

    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string);

    let authorized = provided
        .as_deref()
        .map(|tok| tok.as_bytes().ct_eq(expected.as_bytes()).into())
        .unwrap_or(false);

    if authorized {
        next.run(req).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;
    use tower::ServiceExt;

    async fn make_state_with_token(
        dir: &std::path::Path,
        token: &str,
    ) -> anyhow::Result<std::sync::Arc<AppState>> {
        let mut config = HarnessConfig::default();
        config.server.api_token = Some(token.to_string());
        let thread_manager = crate::thread_manager::ThreadManager::new();
        let server = std::sync::Arc::new(crate::server::HarnessServer::new(
            config,
            thread_manager,
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(
            &harness_core::config::dirs::default_db_path(dir, "tasks"),
        )
        .await?;
        let events = std::sync::Arc::new(harness_observe::event_store::EventStore::new(dir).await?);
        let signal_detector = harness_gc::signal_detector::SignalDetector::new(
            server.config.gc.signal_thresholds.clone().into(),
            harness_core::types::ProjectId::new(),
        );
        let draft_store = harness_gc::draft_store::DraftStore::new(dir)?;
        let gc_agent = std::sync::Arc::new(harness_gc::gc_agent::GcAgent::new(
            server.config.gc.clone(),
            signal_detector,
            draft_store,
            dir.to_path_buf(),
        ));
        let thread_db = crate::thread_db::ThreadDb::open(
            &harness_core::config::dirs::default_db_path(dir, "threads"),
        )
        .await?;
        let _project_svc_tmp = crate::project_registry::ProjectRegistry::open(
            &harness_core::config::dirs::default_db_path(dir, "projects"),
        )
        .await?;
        let project_svc = crate::services::project::DefaultProjectService::new(
            _project_svc_tmp,
            dir.to_path_buf(),
        );
        let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
        let execution_svc = crate::services::execution::DefaultExecutionService::new(
            tasks.clone(),
            server.agent_registry.clone(),
            std::sync::Arc::new(server.config.clone()),
            Default::default(),
            events.clone(),
            vec![],
            None,
            std::sync::Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            None,
            None,
            vec![],
        );
        Ok(std::sync::Arc::new(AppState {
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
                skills: std::sync::Arc::new(tokio::sync::RwLock::new(
                    harness_skills::store::SkillStore::new(),
                )),
                rules: std::sync::Arc::new(tokio::sync::RwLock::new(
                    harness_rules::engine::RuleEngine::new(),
                )),
                gc_agent,
            },
            observability: crate::http::ObservabilityServices {
                events,
                signal_rate_limiter: std::sync::Arc::new(
                    crate::http::rate_limit::SignalRateLimiter::new(100),
                ),
                password_reset_rate_limiter: std::sync::Arc::new(
                    crate::http::rate_limit::PasswordResetRateLimiter::new(5),
                ),
                review_store: None,
            },
            concurrency: crate::http::ConcurrencyServices {
                task_queue: std::sync::Arc::new(crate::task_queue::TaskQueue::new(
                    &Default::default(),
                )),
                workspace_mgr: None,
            },
            runtime_hosts: std::sync::Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
            runtime_project_cache: std::sync::Arc::new(
                crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
            ),
            runtime_state_persist_lock: tokio::sync::Mutex::new(()),
            runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
            notifications: crate::http::NotificationServices {
                notification_tx: tokio::sync::broadcast::channel(32).0,
                notification_lagged_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                    0,
                )),
                notification_lag_log_every: 1,
                notify_tx: None,
                initializing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                initialized: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
            },
            interceptors: vec![],
            intake: crate::http::IntakeServices {
                feishu_intake: None,
                github_pollers: vec![],
                completion_callback: None,
            },
            project_svc,
            task_svc,
            execution_svc,
        }))
    }

    fn make_router(state: std::sync::Arc<AppState>) -> Router {
        Router::new()
            .route("/", get(|| async { "dashboard" }))
            .route("/ws", get(|| async { "ws" }))
            .route("/tasks", get(|| async { "tasks" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                api_auth_middleware,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn auth_middleware_exempts_dashboard() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_state_with_token(dir.path(), "secret").await?;
        let app = make_router(state);

        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn auth_middleware_exempts_ws() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_state_with_token(dir.path(), "secret").await?;
        let app = make_router(state);

        let resp = app
            .oneshot(Request::builder().uri("/ws").body(Body::empty())?)
            .await?;
        // Router returns 200 here (no real WS upgrade in unit test); the key
        // point is it does NOT return 401.
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn auth_middleware_rejects_missing_token() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_state_with_token(dir.path(), "secret").await?;
        let app = make_router(state);

        let resp = app
            .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn auth_middleware_rejects_query_token() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_state_with_token(dir.path(), "secret").await?;
        let app = make_router(state);

        // Query-param token must no longer be accepted for non-exempt paths.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/tasks?token=secret")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn auth_middleware_accepts_bearer_header() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_state_with_token(dir.path(), "secret").await?;
        let app = make_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/tasks")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn auth_middleware_no_token_configured_allows_all() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let thread_manager = crate::thread_manager::ThreadManager::new();
        let server = std::sync::Arc::new(crate::server::HarnessServer::new(
            HarnessConfig::default(),
            thread_manager,
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(
            &harness_core::config::dirs::default_db_path(dir.path(), "tasks"),
        )
        .await?;
        let events =
            std::sync::Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
        let signal_detector = harness_gc::signal_detector::SignalDetector::new(
            server.config.gc.signal_thresholds.clone().into(),
            harness_core::types::ProjectId::new(),
        );
        let draft_store = harness_gc::draft_store::DraftStore::new(dir.path())?;
        let gc_agent = std::sync::Arc::new(harness_gc::gc_agent::GcAgent::new(
            server.config.gc.clone(),
            signal_detector,
            draft_store,
            dir.path().to_path_buf(),
        ));
        let thread_db = crate::thread_db::ThreadDb::open(
            &harness_core::config::dirs::default_db_path(dir.path(), "threads"),
        )
        .await?;
        let _project_svc_tmp = crate::project_registry::ProjectRegistry::open(
            &harness_core::config::dirs::default_db_path(dir.path(), "projects"),
        )
        .await?;
        let project_svc = crate::services::project::DefaultProjectService::new(
            _project_svc_tmp,
            dir.path().to_path_buf(),
        );
        let task_svc = crate::services::task::DefaultTaskService::new(tasks.clone());
        let execution_svc = crate::services::execution::DefaultExecutionService::new(
            tasks.clone(),
            server.agent_registry.clone(),
            std::sync::Arc::new(server.config.clone()),
            Default::default(),
            events.clone(),
            vec![],
            None,
            std::sync::Arc::new(crate::task_queue::TaskQueue::new(&Default::default())),
            None,
            None,
            vec![],
        );
        let state = std::sync::Arc::new(AppState {
            core: crate::http::CoreServices {
                server,
                project_root: dir.path().to_path_buf(),
                home_dir: dir.path().to_path_buf(),
                tasks,
                thread_db: Some(thread_db),
                plan_db: None,
                plan_cache: std::sync::Arc::new(dashmap::DashMap::new()),
                project_registry: None,
                runtime_state_store: None,
                q_values: None,
            },
            engines: crate::http::EngineServices {
                skills: std::sync::Arc::new(tokio::sync::RwLock::new(
                    harness_skills::store::SkillStore::new(),
                )),
                rules: std::sync::Arc::new(tokio::sync::RwLock::new(
                    harness_rules::engine::RuleEngine::new(),
                )),
                gc_agent,
            },
            observability: crate::http::ObservabilityServices {
                events,
                signal_rate_limiter: std::sync::Arc::new(
                    crate::http::rate_limit::SignalRateLimiter::new(100),
                ),
                password_reset_rate_limiter: std::sync::Arc::new(
                    crate::http::rate_limit::PasswordResetRateLimiter::new(5),
                ),
                review_store: None,
            },
            concurrency: crate::http::ConcurrencyServices {
                task_queue: std::sync::Arc::new(crate::task_queue::TaskQueue::new(
                    &Default::default(),
                )),
                workspace_mgr: None,
            },
            runtime_hosts: std::sync::Arc::new(crate::runtime_hosts::RuntimeHostManager::new()),
            runtime_project_cache: std::sync::Arc::new(
                crate::runtime_project_cache::RuntimeProjectCacheManager::new(),
            ),
            runtime_state_persist_lock: tokio::sync::Mutex::new(()),
            runtime_state_dirty: std::sync::atomic::AtomicBool::new(false),
            notifications: crate::http::NotificationServices {
                notification_tx: tokio::sync::broadcast::channel(32).0,
                notification_lagged_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                    0,
                )),
                notification_lag_log_every: 1,
                notify_tx: None,
                initializing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                initialized: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                ws_shutdown_tx: tokio::sync::broadcast::channel(1).0,
            },
            interceptors: vec![],
            intake: crate::http::IntakeServices {
                feishu_intake: None,
                github_pollers: vec![],
                completion_callback: None,
            },
            project_svc,
            task_svc,
            execution_svc,
        });
        let app = make_router(state);

        let resp = app
            .oneshot(Request::builder().uri("/tasks").body(Body::empty())?)
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        Ok(())
    }
}
