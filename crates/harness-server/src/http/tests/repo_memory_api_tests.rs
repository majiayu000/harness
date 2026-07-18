use super::*;
use crate::handlers::repo_memory_api::{
    delete_repo_memory_record_route, list_project_repo_memory_route,
};
use axum::{http::Method, routing::delete};
use harness_workflow::runtime::{
    RepoMemoryKind, RepoMemoryOutcome, RepoMemoryRecord, RepoMemoryRetrievalOptions,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};

#[tokio::test]
async fn memory_api_lists_repo_and_prunes_from_retrieval() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let keep = store
        .insert_repo_memory_record(
            &RepoMemoryRecord::new(
                "owner/repo",
                "implement_issue",
                RepoMemoryOutcome::Done,
                RepoMemoryKind::ValidationCommand,
                json!({"validation": [{"command": "cargo test", "status": "passed"}]}),
            )
            .with_evidence_ref("workflow:run-1:event:event-1"),
        )
        .await?;
    let prune = store
        .insert_repo_memory_record(
            &RepoMemoryRecord::new(
                "owner/repo",
                "implement_issue",
                RepoMemoryOutcome::Failed,
                RepoMemoryKind::FailureLesson,
                json!({"failure_class": "ci_warning", "lesson": "run clippy before pushing"}),
            )
            .with_evidence_ref("workflow:run-2:event:event-2"),
        )
        .await?;
    store
        .insert_repo_memory_record(&RepoMemoryRecord::new(
            "owner/other",
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::EnvironmentNote,
            json!({"note": "other repo"}),
        ))
        .await?;
    let app = memory_api_app(state.clone());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/projects/owner%2Frepo/memory")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["repo"], "owner/repo");
    let records = body["records"]
        .as_array()
        .expect("records should be an array");
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .any(|record| record["id"] == keep.id.to_string()));
    assert!(records
        .iter()
        .any(|record| record["id"] == prune.id.to_string()));
    assert!(records.iter().all(|record| record["repo"] == "owner/repo"));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/memory/{}", prune.id))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["deleted"], true);
    assert_eq!(body["id"], prune.id.to_string());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/memory/not-a-uuid")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/projects/owner%2Frepo/memory")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    let records = body["records"]
        .as_array()
        .expect("records should be an array");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["id"], keep.id.to_string());

    let retrieved = store
        .retrieve_repo_memory_records(
            "owner/repo",
            "implement_issue",
            RepoMemoryRetrievalOptions::default(),
        )
        .await?;
    assert!(retrieved.iter().any(|entry| entry.record.id == keep.id));
    assert!(retrieved.iter().all(|entry| entry.record.id != prune.id));
    Ok(())
}

#[tokio::test]
async fn memory_api_rejects_missing_store() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let app = memory_api_app(state);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/projects/owner%2Frepo/memory")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/memory/not-a-uuid")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    Ok(())
}

fn memory_api_app(state: std::sync::Arc<crate::http::AppState>) -> Router {
    Router::new()
        .route(
            "/api/projects/{id}/memory",
            get(list_project_repo_memory_route),
        )
        .route("/api/memory/{id}", delete(delete_repo_memory_record_route))
        .with_state(state)
}

async fn response_json(response: axum::response::Response) -> anyhow::Result<Value> {
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}
