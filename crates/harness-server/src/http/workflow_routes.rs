use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::future::Future;
use std::sync::Arc;

use super::state::AppState;

#[path = "misc_routes_runtime_tree.rs"]
mod runtime_tree;
pub(crate) use runtime_tree::get_workflow_runtime_tree;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueWorkflowByIssueQuery {
    pub project_id: String,
    pub repo: Option<String>,
    pub issue: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueWorkflowByPrQuery {
    pub project_id: String,
    pub repo: Option<String>,
    pub pr: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ProjectWorkflowByProjectQuery {
    pub project_id: String,
    pub repo: Option<String>,
}

/// Shared response mapping for the workflow REST lookup handlers: serialize a
/// hit, 404 on a miss, 500 with the store error otherwise.
async fn workflow_lookup_response<T, F>(entity: &'static str, lookup: F) -> Response
where
    T: serde::Serialize,
    F: Future<Output = anyhow::Result<Option<T>>>,
{
    match lookup.await {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("{entity} not found") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn workflow_lookup_store_unavailable(entity: &'static str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": format!("{entity} store unavailable") })),
    )
        .into_response()
}

pub(crate) async fn get_issue_workflow_by_issue(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssueWorkflowByIssueQuery>,
) -> Response {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return workflow_lookup_store_unavailable("issue workflow");
    };
    workflow_lookup_response(
        "issue workflow",
        store.get_by_issue(&query.project_id, query.repo.as_deref(), query.issue),
    )
    .await
}

pub(crate) async fn get_issue_workflow_by_pr(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssueWorkflowByPrQuery>,
) -> Response {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return workflow_lookup_store_unavailable("issue workflow");
    };
    workflow_lookup_response(
        "issue workflow",
        store.get_by_pr(&query.project_id, query.repo.as_deref(), query.pr),
    )
    .await
}

pub(crate) async fn get_project_workflow_by_project(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProjectWorkflowByProjectQuery>,
) -> Response {
    let Some(store) = state.core.project_workflow_store.as_ref() else {
        return workflow_lookup_store_unavailable("project workflow");
    };
    workflow_lookup_response(
        "project workflow",
        store.get_by_project(&query.project_id, query.repo.as_deref()),
    )
    .await
}
