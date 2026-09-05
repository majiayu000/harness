use crate::http::rest_contract::{ContractJson as Json, ContractQuery as Query};
use crate::http::AppState;
use axum::{extract::State, http::StatusCode};
use harness_protocol::rest::{ReconcileParams, ReconciliationReport};
use std::sync::Arc;

/// POST /reconcile[?dry_run=true]
///
/// Runs one reconciliation tick against GitHub and returns the report as JSON.
/// Pass `?dry_run=true` to see what would change without applying transitions.
pub async fn handle(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ReconcileParams>,
) -> Result<Json<ReconciliationReport>, StatusCode> {
    let report = crate::reconciliation::run_once_with_runtime_config(
        state.core.workflow_runtime_store.as_deref(),
        state.core.issue_workflow_store.as_deref(),
        &state.core.server.config.reconciliation,
        params.dry_run,
        state.core.server.config.server.github_token.as_deref(),
    )
    .await;

    Ok(Json(report))
}
