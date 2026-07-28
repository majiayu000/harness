use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::state::AppState;

fn startup_error_code(error: Option<&str>) -> Option<&'static str> {
    let error = error?;
    let lower = error.to_ascii_lowercase();
    if lower.contains("migration") {
        Some("migration_failed")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        Some("timeout")
    } else if lower.contains("connection")
        || lower.contains("connect")
        || lower.contains("database")
        || lower.contains("postgres")
        || lower.contains("pool")
    {
        Some("database_unavailable")
    } else {
        Some("startup_failed")
    }
}

pub(crate) async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.core.tasks.count();
    let dirty = state.is_runtime_state_dirty();
    let degraded = &state.degraded_subsystems;
    let runtime_logs = &state.core.server.runtime_logs;
    let circuit_breakers = state.runtime_circuit_breakers.snapshots(chrono::Utc::now());
    let runtime_degraded = circuit_breakers
        .iter()
        .any(|breaker| breaker.state != "closed");
    let postgres_catalog = state.postgres_catalog.snapshot().await;
    let postgres_catalog_degraded = postgres_catalog.threshold_breached;
    let unavailable_required_tiers = state
        .isolation_availability
        .unavailable_required_tiers(&state.core.server.config.isolation);
    let isolation_degraded = !unavailable_required_tiers.is_empty();
    let startup_statuses: Vec<serde_json::Value> = state
        .startup_statuses
        .iter()
        .map(|status| {
            json!({
                "name": status.name,
                "critical": status.is_critical(),
                "ready": status.ready,
                "error": startup_error_code(status.error.as_deref()),
            })
        })
        .collect();
    let status = if degraded.is_empty()
        && !dirty
        && !runtime_degraded
        && !isolation_degraded
        && !postgres_catalog_degraded
    {
        "ok"
    } else {
        "degraded"
    };
    Json(json!({
        "status": status,
        "tasks": count,
        "persistence": {
            "degraded_subsystems": degraded,
            "runtime_state_dirty": dirty,
            "startup": {
                "stores": startup_statuses,
            }
        },
        "runtime_logs": {
            "state": runtime_logs.state.as_str(),
            "path_hint": runtime_logs.path_hint.clone(),
            "retention_days": runtime_logs.retention_days,
            "retention_max_files": runtime_logs.retention_max_files,
        },
        "runtime": {
            "circuit_breakers": circuit_breakers,
        },
        "postgres_catalog": postgres_catalog,
        "isolation": {
            "tiers": state.isolation_availability.tiers.clone(),
            "unavailable_required_tiers": unavailable_required_tiers,
        }
    }))
}

/// GET /projects/queue-stats — per-project queue stats alongside the global queue summary.
pub(crate) async fn project_queue_stats(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let tq = &state.concurrency.task_queue;
    let active_counts = crate::handlers::overview::active_task_overview_counts(&state).await;
    let queue_project_stats = tq.all_project_stats();
    let mut project_ids: BTreeSet<String> = active_counts.by_project.keys().cloned().collect();
    project_ids.extend(queue_project_stats.iter().map(|(id, _)| id.clone()));
    let projects: serde_json::Map<String, serde_json::Value> = project_ids
        .into_iter()
        .map(|id| {
            let active = active_counts
                .by_project
                .get(&id)
                .copied()
                .unwrap_or_default();
            let limit = queue_project_stats
                .iter()
                .find(|(project_id, _)| project_id == &id)
                .map(|(_, stats)| stats.limit)
                .unwrap_or_else(|| tq.project_stats(&id).limit);
            (
                id,
                json!({
                    "running": active.running,
                    "queued": active.queued,
                    "limit": limit,
                }),
            )
        })
        .collect();
    Json(json!({
        "global": {
            "running": active_counts.running,
            "queued": active_counts.queued,
            "limit": tq.global_limit(),
        },
        "projects": projects,
    }))
}

pub(crate) async fn reset_runtime_circuit_breaker(
    State(state): State<Arc<AppState>>,
    Path(profile): Path<String>,
) -> Response {
    let profile = profile.trim();
    if profile.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "runtime profile is required" })),
        )
            .into_response();
    }
    let event = state
        .runtime_circuit_breakers
        .reset(profile, chrono::Utc::now());
    crate::workflow_runtime_worker::emit_circuit_breaker_events(&state, vec![event]).await;
    let circuit_breakers = state.runtime_circuit_breakers.snapshots(chrono::Utc::now());
    (
        StatusCode::OK,
        Json(json!({
            "profile": profile,
            "circuit_breakers": circuit_breakers,
        })),
    )
        .into_response()
}
