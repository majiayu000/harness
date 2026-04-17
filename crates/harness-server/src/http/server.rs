use super::{handlers, intake_auth, startup_recovery, task_routes, types::AppState};
use crate::server::HarnessServer;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    tracing::info!("harness: HTTP server listening on {addr}");
    // Record true server start time before accepting any connections.
    crate::handlers::dashboard::SERVER_START.get_or_init(std::time::Instant::now);

    let state = Arc::new(super::init::build_app_state(server.clone()).await?);

    // Startup summary — one clean line instead of scattered logs.
    {
        let guard_count = state.engines.rules.read().await.guards().len();
        let skill_count = state.engines.skills.read().await.list().len();
        let task_count = state.core.tasks.list_all().len();
        tracing::info!(
            project = %state.core.project_root.display(),
            guards = guard_count,
            skills = skill_count,
            pending_tasks = task_count,
            "harness: ready"
        );
    }

    startup_recovery::spawn_awaiting_deps_watcher(&state);
    startup_recovery::redispatch_recovered_pr_tasks(&state);
    startup_recovery::redispatch_checkpoint_tasks(&state).await;

    let initial_grade = {
        let events = state
            .observability
            .events
            .query(&harness_core::types::EventFilters::default())
            .await
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
    // Pass the pre-built GitHub pollers from AppState to the orchestrator so
    // both share the same Arc instances and on_task_complete operates on the
    // live poller's dispatched map.
    let github_sources = state.intake.github_pollers.clone();
    crate::intake::build_orchestrator(
        &state.core.server.config.intake,
        Some(&super::init::expand_tilde(
            &state.core.server.config.server.data_dir,
        )),
        state.intake.feishu_intake.clone(),
        github_sources,
    )
    .start(state.clone());

    let app = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let ws_shutdown_tx = state.notifications.ws_shutdown_tx.clone();
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("server shutting down: closing WebSocket connections");
            ws_shutdown_tx.send(()).ok();
        })
        .await;
    tracing::info!("server shutting down");
    state.observability.events.shutdown().await;
    serve_result?;
    Ok(())
}

fn build_router(state: Arc<AppState>) -> Router {
    let max_body = state.core.server.config.server.max_webhook_body_bytes;
    Router::new()
        .route("/", get(crate::dashboard::index))
        .route("/favicon.ico", get(crate::dashboard::favicon))
        .route("/health", get(handlers::health_check))
        .route("/rpc", post(handlers::handle_rpc))
        .route("/ws", get(crate::websocket::ws_handler))
        .route("/tasks", post(task_routes::create_task))
        .route("/tasks", get(handlers::list_tasks))
        .route("/tasks/batch", post(task_routes::create_tasks_batch))
        .route("/tasks/{id}", get(handlers::get_task))
        .route("/tasks/{id}/cancel", post(task_routes::cancel_task))
        .route("/tasks/{id}/artifacts", get(handlers::get_task_artifacts))
        .route("/tasks/{id}/stream", get(handlers::stream_task_sse))
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
        .route("/projects/queue-stats", get(handlers::project_queue_stats))
        .route("/api/dashboard", get(crate::handlers::dashboard::dashboard))
        .route("/api/intake", get(intake_auth::intake_status))
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
            post(handlers::github_webhook).layer(DefaultBodyLimit::max(max_body)),
        )
        .route(
            "/webhook/feishu",
            post(crate::intake::feishu::feishu_webhook).layer(DefaultBodyLimit::max(max_body)),
        )
        .route(
            "/signals",
            post(intake_auth::ingest_signal).layer(DefaultBodyLimit::max(max_body)),
        )
        .route("/auth/reset-password", post(intake_auth::password_reset))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            super::auth::api_auth_middleware,
        ))
        .with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!("failed to install SIGTERM handler: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}
