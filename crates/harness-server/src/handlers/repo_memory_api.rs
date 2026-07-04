use crate::http::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use harness_workflow::runtime::RepoMemoryRecord;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

pub async fn list_project_repo_memory_route(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "workflow runtime store unavailable" })),
        );
    };
    let repo = repo.trim();
    if repo.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "repo must not be empty" })),
        );
    }

    match store.list_repo_memory_records(repo).await {
        Ok(records) => (
            StatusCode::OK,
            Json(json!({
                "repo": repo,
                "records": records.into_iter().map(repo_memory_record_json).collect::<Vec<_>>(),
            })),
        ),
        Err(error) => {
            tracing::error!(repo, "failed to list repo memory records: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to list repo memory records" })),
            )
        }
    }
}

pub async fn delete_repo_memory_record_route(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "workflow runtime store unavailable" })),
        );
    };
    let id = match Uuid::parse_str(id.trim()) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "memory id must be a UUID" })),
            )
        }
    };

    match store.delete_repo_memory_record(id).await {
        Ok(true) => (StatusCode::OK, Json(json!({ "deleted": true, "id": id }))),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "repo memory record not found", "id": id })),
        ),
        Err(error) => {
            tracing::error!(%id, "failed to delete repo memory record: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to delete repo memory record" })),
            )
        }
    }
}

fn repo_memory_record_json(record: RepoMemoryRecord) -> Value {
    json!({
        "id": record.id,
        "repo": record.repo,
        "activity_class": record.activity_class,
        "outcome": record.outcome.db_value(),
        "kind": record.kind.db_value(),
        "payload": record.payload_json,
        "evidence_ref": record.evidence_ref,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
        "last_used_at": record.last_used_at,
        "use_count": record.use_count,
    })
}
