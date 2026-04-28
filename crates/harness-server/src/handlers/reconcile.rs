use crate::http::AppState;
use axum::{extract::Query, extract::State, http::StatusCode, Json};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct ReconcileParams {
    #[serde(default)]
    pub dry_run: bool,
}

/// POST /reconcile[?dry_run=true]
///
/// Runs one reconciliation tick against GitHub and returns the report as JSON.
/// Pass `?dry_run=true` to see what would change without applying transitions.
pub async fn handle(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ReconcileParams>,
) -> Result<Json<crate::reconciliation::ReconciliationReport>, StatusCode> {
    let max_calls = state
        .core
        .server
        .config
        .reconciliation
        .max_gh_calls_per_minute;

    let report =
        crate::reconciliation::run_once(&state.core.tasks, max_calls, params.dry_run).await;

    Ok(Json(report))
}
