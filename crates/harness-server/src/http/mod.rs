use crate::server::HarnessServer;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// Items re-exported into test scope via `use super::*` in tests.rs.
#[cfg(test)]
use crate::task_runner;
#[cfg(test)]
use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    routing::{get, post},
    Router,
};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicU64};

pub(crate) mod auth;
pub(crate) mod auto_merge;
pub(crate) mod background;
pub(crate) mod builders;
pub(crate) mod http_router;
pub(crate) mod init;
pub(crate) mod misc_routes;
mod orphan_reaper;
pub(crate) mod pr_hygiene_background;
pub(crate) mod rate_limit;
pub(crate) mod sse_routes;
pub(crate) mod state;
pub(crate) mod task_mutation_routes;
pub(crate) mod task_query_routes;
pub(crate) mod task_routes;
pub(crate) mod task_submission_routes;

#[cfg(test)]
mod reviewer_resolution_tests;
#[cfg(test)]
mod shutdown_test;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod test_fixtures;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_password_reset;

// Re-export all public symbols so callers using `crate::http::*` paths continue to work.
pub use init::build_app_state;
pub use state::{
    AppState, ConcurrencyServices, CoreServices, EngineServices,
    GitHubTokenDispatchCounterSnapshot, GitHubTokenDispatchMetric, IntakeServices,
    NotificationServices, ObservabilityServices,
};

// Handler re-exports — moved to focused submodules, kept accessible via `crate::http::`.
pub(crate) use misc_routes::{
    get_issue_workflow_by_issue, get_issue_workflow_by_pr, get_project_workflow_by_project,
    get_workflow_runtime_tree, github_webhook, handle_rpc, health_check, ingest_signal,
    intake_status, password_reset, project_queue_stats,
};
pub(crate) use sse_routes::stream_task_sse;
pub(crate) use task_query_routes::get_task_proof;
pub(crate) use task_submission_routes::{
    get_task, get_task_artifacts, get_task_prompts, list_tasks,
};

/// Resolve the reviewer agent for independent agent review.
///
/// 1. If `config.reviewer_agent` is set, use it.
/// 2. Otherwise, auto-select the first registered agent that isn't the implementor.
/// 3. If none found, return None (agent review will be skipped).
pub(crate) fn resolve_reviewer(
    registry: &harness_agents::registry::AgentRegistry,
    config: &harness_core::config::agents::AgentReviewConfig,
    implementor_name: &str,
) -> (
    Option<Arc<dyn harness_core::agent::CodeAgent>>,
    harness_core::config::agents::AgentReviewConfig,
) {
    if !config.enabled {
        return (None, config.clone());
    }

    // Explicit reviewer
    if !config.reviewer_agent.is_empty() {
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

/// Extract the PR number from a GitHub PR URL.
///
/// Handles:
/// - `.../pull/42`
/// - `.../pull/42/files`
/// - `.../pull/42#discussion_r...`
pub(crate) fn parse_pr_num_from_url(url: &str) -> Option<u64> {
    // Strip fragment first, then query string
    let url = url.split('#').next().unwrap_or(url);
    let url = url.split('?').next().unwrap_or(url);
    // Walk path segments looking for "pull", then parse the segment that follows
    let mut parts = url.split('/');
    while let Some(seg) = parts.next() {
        if seg == "pull" {
            return parts.next()?.parse::<u64>().ok();
        }
    }
    None
}

pub async fn serve(server: Arc<HarnessServer>, addr: SocketAddr) -> anyhow::Result<()> {
    // Record true server start time before accepting any connections.
    crate::handlers::dashboard::SERVER_START.get_or_init(std::time::Instant::now);

    let state = Arc::new(build_app_state(server.clone()).await?);
    let app = http_router::build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("harness: HTTP server listening on {addr}");
    let ws_shutdown_tx = state.notifications.ws_shutdown_tx.clone();
    let shutdown_cfg = state.core.server.config.server.shutdown.clone();

    // Fan the first SIGINT/SIGTERM out to two consumers:
    //   (a) the with_graceful_shutdown future, so Axum stops accepting new
    //       connections the instant the signal lands (P1 fix);
    //   (b) the force-watcher task, so the hard-deadline timer starts at
    //       signal time, not at server startup (P0 fix).
    // A watch channel lets a single signal handler notify both consumers
    // without racing on which side runs first.
    let (first_signal_tx, first_signal_rx_serve) = tokio::sync::watch::channel(false);
    let first_signal_rx_force = first_signal_tx.subscribe();
    tokio::spawn(async move {
        wait_first_termination_signal().await;
        if first_signal_tx.send(true).is_err() {
            tracing::debug!("shutdown signal had no active HTTP receivers");
        }
    });

    let serve_future = axum::serve(listener, app).with_graceful_shutdown(
        axum_graceful_shutdown_signal(first_signal_rx_serve, shutdown_cfg.clone()),
    );

    let force_watcher_cfg = shutdown_cfg.clone();
    let force_watcher_events = state.observability.events.clone();
    let force_watcher_ws_tx = ws_shutdown_tx.clone();
    let force_watcher = tokio::spawn(async move {
        let Some(reason) = wait_for_first_signal_then_drain_or_force(
            first_signal_rx_force,
            force_watcher_cfg.clone(),
            wait_second_termination_signal(),
        )
        .await
        else {
            return;
        };
        tracing::info!(
            ?reason,
            "shutdown: drain phase ended, force-closing long-lived connections"
        );
        force_watcher_ws_tx.send(()).ok();

        // Phase 3: hard deadline. If the serve future has not resolved within
        // the force-grace window, exit forcefully.
        tokio::time::sleep(Duration::from_secs(force_watcher_cfg.force_grace_secs)).await;
        tracing::error!(
            force_grace_secs = force_watcher_cfg.force_grace_secs,
            "shutdown: force grace exceeded — process::exit(1)"
        );
        force_watcher_events.shutdown().await;
        std::process::exit(1);
    });

    let serve_handle = tokio::spawn(async move { serve_future.await });
    tokio::task::yield_now().await;

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

    // Spawn background watcher for AwaitingDeps tasks.
    background::spawn_awaiting_deps_watcher(&state);

    // Run one reconciliation tick against GitHub before any recovery so that
    // recovery decisions are made on fresh GitHub truth.
    if state.core.server.config.reconciliation.enabled {
        crate::reconciliation::run_once_with_runtime_config(
            &state.core.tasks,
            state.core.workflow_runtime_store.as_deref(),
            state.core.issue_workflow_store.as_deref(),
            &state.core.server.config.reconciliation,
            false,
            state.core.server.config.server.github_token.as_deref(),
        )
        .await;
    } else {
        tracing::info!("startup reconciliation disabled by config");
    }

    // Re-dispatch tasks that were recovered to pending after server restart.
    // These had PRs when the server crashed and need their review loop re-started.
    background::spawn_pr_recovery(&state);

    // Re-dispatch recovered review/planner tasks that have restart-safe input bundles.
    background::spawn_system_task_recovery(&state);

    // Re-dispatch tasks recovered from plan/triage checkpoints but without a PR.
    background::spawn_checkpoint_recovery(&state).await;

    // Re-dispatch leftover pending tasks that crashed before their first checkpoint.
    background::spawn_orphan_pending_recovery(&state).await;

    // Periodically sweep runtime issue workflows with attached PRs and emit
    // workflow command outbox rows.
    background::spawn_runtime_pr_feedback_sweeper(&state);

    // Periodically inspect managed open PRs for stale DIRTY/BEHIND mergeability
    // and route repair through workflow-owned PR feedback activities.
    pr_hygiene_background::spawn_runtime_pr_hygiene_sweeper(&state);

    // Periodically reap orphaned path-derived Postgres schemas whose owning
    // workspace directory has been removed, bounding catalog growth (storage RFC).
    orphan_reaper::spawn_orphan_schema_reaper(&state);

    // Convert workflow command outbox rows into runtime jobs when the workflow
    // policy keeps the dispatcher enabled.
    background::spawn_runtime_command_dispatcher(&state);

    // Execute pending workflow runtime jobs through registered agent runtimes
    // when the workflow policy keeps the worker enabled.
    background::spawn_runtime_job_workers(&state);

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
    let github_sources = state
        .intake
        .github_pollers
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, source)| {
            crate::intake::IntakeSourceRegistration::new(
                source,
                state.intake.github_poller_repos.get(index).cloned(),
            )
        })
        .collect();
    crate::intake::build_orchestrator(
        &state.core.server.config.intake,
        state.intake.feishu_intake.clone(),
        github_sources,
    )
    .start(state.clone());

    let serve_result = serve_handle
        .await
        .map_err(|error| anyhow::anyhow!("HTTP server task failed: {error}"))?;
    tracing::info!("server shutting down");
    ws_shutdown_tx.send(()).ok();
    state.observability.events.shutdown().await;
    force_watcher.abort();
    serve_result?;
    Ok(())
}

async fn await_first_shutdown_signal(mut signal_rx: tokio::sync::watch::Receiver<bool>) -> bool {
    loop {
        if *signal_rx.borrow_and_update() {
            return true;
        }
        if signal_rx.changed().await.is_err() {
            return false;
        }
    }
}

async fn axum_graceful_shutdown_signal(
    signal_rx: tokio::sync::watch::Receiver<bool>,
    shutdown_cfg: harness_core::config::shutdown::ShutdownConfig,
) {
    if !await_first_shutdown_signal(signal_rx).await {
        return;
    }
    tracing::info!(
        drain_deadline_secs = shutdown_cfg.drain_timeout_secs,
        progress_log_secs = shutdown_cfg.progress_log_secs,
        "shutdown: draining; press Ctrl+C again to force"
    );
}

async fn wait_for_first_signal_then_drain_or_force<F>(
    signal_rx: tokio::sync::watch::Receiver<bool>,
    shutdown_cfg: harness_core::config::shutdown::ShutdownConfig,
    user_force: F,
) -> Option<ShutdownReason>
where
    F: std::future::Future<Output = ()>,
{
    if !await_first_shutdown_signal(signal_rx).await {
        return None;
    }
    Some(
        drain_or_force(
            shutdown_cfg.drain_timeout_secs,
            shutdown_cfg.progress_log_secs,
            user_force,
        )
        .await,
    )
}

/// Reason a graceful shutdown ended. Surfaced for observability and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    /// Drain deadline expired before all in-flight work finished.
    DrainTimeout,
    /// User pressed Ctrl+C (or sent SIGTERM) a second time during drain.
    UserForced,
}

/// Wait for the first SIGINT/SIGTERM. Errors installing the handlers are
/// logged but do not abort the drain — we still want orderly shutdown when
/// only one signal source is available.
async fn wait_first_termination_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to install Ctrl+C handler: {e}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
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

/// Wait for a second SIGINT/SIGTERM after the first one has already been
/// consumed. Re-installs fresh handlers so a follow-up Ctrl+C is observed.
async fn wait_second_termination_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to re-install Ctrl+C handler: {e}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("failed to re-install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::warn!("shutdown: second Ctrl+C — forcing"),
        _ = terminate => tracing::warn!("shutdown: second SIGTERM — forcing"),
    }
}

/// Run the drain phase: emit progress logs every `progress_log_secs`,
/// stop when either the drain deadline expires or the user-force future
/// resolves.
async fn drain_or_force<F>(
    drain_timeout_secs: u64,
    progress_log_secs: u64,
    user_force: F,
) -> ShutdownReason
where
    F: std::future::Future<Output = ()>,
{
    let drain_deadline = tokio::time::sleep(Duration::from_secs(drain_timeout_secs));
    let progress_interval = Duration::from_secs(progress_log_secs.max(1)); // never spin at 0s
    let mut progress = tokio::time::interval(progress_interval);
    // Skip the immediate first tick so the user does not see a duplicate
    // log right after the "draining" line above.
    progress.tick().await;

    tokio::pin!(drain_deadline);
    tokio::pin!(user_force);

    loop {
        tokio::select! {
            biased;
            _ = &mut user_force => return ShutdownReason::UserForced,
            _ = &mut drain_deadline => return ShutdownReason::DrainTimeout,
            _ = progress.tick() => {
                tracing::info!(
                    drain_timeout_secs,
                    "shutdown: still draining"
                );
            }
        }
    }
}
