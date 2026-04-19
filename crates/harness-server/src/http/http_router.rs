use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
    Router,
};
use std::sync::Arc;

use super::{
    auth, get_task, get_task_artifacts, get_task_prompts, github_webhook, handle_rpc, health_check,
    ingest_signal, intake_status, list_tasks, password_reset, project_queue_stats, state::AppState,
    stream_task_sse, task_routes,
};

pub(super) fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(crate::dashboard::index))
        .route("/overview", get(crate::overview::index))
        .route(
            "/assets/{filename}",
            axum::routing::get(crate::assets::serve),
        )
        .route("/favicon.ico", get(crate::dashboard::favicon))
        .route("/health", get(health_check))
        .route("/rpc", post(handle_rpc))
        .route("/ws", get(crate::websocket::ws_handler))
        .route("/tasks", post(task_routes::create_task))
        .route("/tasks", get(list_tasks))
        .route("/tasks/batch", post(task_routes::create_tasks_batch))
        .route("/tasks/{id}", get(get_task))
        .route("/tasks/{id}/cancel", post(task_routes::cancel_task))
        .route("/tasks/{id}/artifacts", get(get_task_artifacts))
        .route("/tasks/{id}/prompts", get(get_task_prompts))
        .route("/tasks/{id}/stream", get(stream_task_sse))
        // /api/tasks mirrors /tasks for web-frontend consistency with other /api/* routes.
        .route("/api/tasks", get(list_tasks))
        .route(
            "/projects",
            post(crate::handlers::projects::register_project)
                .get(crate::handlers::projects::list_projects),
        )
        .route(
            "/projects/{id}",
            get(crate::handlers::projects::get_project)
                .delete(crate::handlers::projects::delete_project),
        )
        .route("/projects/queue-stats", get(project_queue_stats))
        .route("/api/dashboard", get(crate::handlers::dashboard::dashboard))
        .route("/api/overview", get(crate::handlers::overview::overview))
        .route("/api/intake", get(intake_status))
        .route(
            "/api/runtime-hosts",
            get(crate::handlers::runtime_hosts::list_runtime_hosts),
        )
        .route(
            "/api/runtime-hosts/register",
            post(crate::handlers::runtime_hosts::register_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/heartbeat",
            post(crate::handlers::runtime_hosts::heartbeat_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/deregister",
            post(crate::handlers::runtime_hosts::deregister_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/tasks/claim",
            post(crate::handlers::runtime_hosts::claim_task_for_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/projects",
            get(crate::handlers::runtime_project_cache::list_runtime_host_projects),
        )
        .route(
            "/api/runtime-hosts/{host_id}/projects/sync",
            post(crate::handlers::runtime_project_cache::sync_runtime_host_projects),
        )
        .route(
            "/api/token-usage",
            get(crate::handlers::token_usage::token_usage),
        )
        .route(
            "/webhook",
            post(github_webhook).layer(DefaultBodyLimit::max(
                state.core.server.config.server.max_webhook_body_bytes,
            )),
        )
        .route(
            "/webhook/feishu",
            post(crate::intake::feishu::feishu_webhook).layer(DefaultBodyLimit::max(
                state.core.server.config.server.max_webhook_body_bytes,
            )),
        )
        .route(
            "/signals",
            post(ingest_signal).layer(DefaultBodyLimit::max(
                state.core.server.config.server.max_webhook_body_bytes,
            )),
        )
        .route("/auth/reset-password", post(password_reset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::api_auth_middleware,
        ))
        .with_state(state)
}
