use anyhow::Context as _;
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use harness_eval::PrRepairEvalInput;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::eval_store::{AddEvalArtifact, CreateEvalRun, EvalStore};
use crate::http::AppState;

#[derive(Debug, Deserialize)]
pub struct EvalListQuery {
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ScoreEvalRunRequest {
    pub input: Option<PrRepairEvalInput>,
}

pub async fn create_eval_run(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateEvalRun>,
) -> (StatusCode, Json<Value>) {
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.create_run(req).await {
        Ok(run) => (StatusCode::CREATED, Json(json!({ "run": run }))),
        Err(error) => internal_error("failed to create eval run", error),
    }
}

pub async fn list_eval_runs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EvalListQuery>,
) -> (StatusCode, Json<Value>) {
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.list_runs(query.limit.unwrap_or(50)).await {
        Ok(runs) => (StatusCode::OK, Json(json!({ "runs": runs }))),
        Err(error) => internal_error("failed to list eval runs", error),
    }
}

pub async fn get_eval_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.get_run(&run_id).await {
        Ok(Some(run)) => (StatusCode::OK, Json(json!({ "run": run }))),
        Ok(None) => not_found("eval run not found"),
        Err(error) => internal_error("failed to get eval run", error),
    }
}

pub async fn add_eval_artifact(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(mut req): Json<AddEvalArtifact>,
) -> (StatusCode, Json<Value>) {
    req.artifact_type = req.artifact_type.trim().to_string();
    if req.artifact_type.is_empty() {
        return bad_request("artifact_type must not be empty");
    }
    if req.body.is_empty() {
        return bad_request("body must not be empty");
    }
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.add_artifact(&run_id, req).await {
        Ok(Some(artifact)) => (StatusCode::CREATED, Json(json!({ "artifact": artifact }))),
        Ok(None) => not_found("eval run not found"),
        Err(error) => internal_error("failed to add eval artifact", error),
    }
}

pub async fn list_eval_artifacts(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.get_run(&run_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found("eval run not found"),
        Err(error) => return internal_error("failed to get eval run", error),
    }
    match store.list_artifacts(&run_id).await {
        Ok(artifacts) => (StatusCode::OK, Json(json!({ "artifacts": artifacts }))),
        Err(error) => internal_error("failed to list eval artifacts", error),
    }
}

pub async fn score_eval_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let req = match parse_score_eval_run_request(&body) {
        Ok(req) => req,
        Err(error) => return bad_request(error),
    };
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.get_run(&run_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found("eval run not found"),
        Err(error) => return internal_error("failed to get eval run", error),
    }
    let input = match req.input {
        Some(input) => input,
        None => match load_input_artifact(&store, &run_id).await {
            Ok(Some(input)) => input,
            Ok(None) => {
                return bad_request(
                    "score input missing and no pr_repair_eval_input artifact was found",
                )
            }
            Err(error) if is_request_error(&error) => return bad_request(error.to_string()),
            Err(error) => return internal_error("failed to load eval input artifact", error),
        },
    };
    match store.score_run(&run_id, input).await {
        Ok(Some(result)) => (StatusCode::OK, Json(json!(result))),
        Ok(None) => not_found("eval run not found"),
        Err(error) if is_request_error(&error) => bad_request(error.to_string()),
        Err(error) => internal_error("failed to score eval run", error),
    }
}

fn parse_score_eval_run_request(body: &Bytes) -> Result<ScoreEvalRunRequest, String> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(ScoreEvalRunRequest { input: None });
    }
    serde_json::from_slice(body).map_err(|error| format!("invalid score request body: {error}"))
}

pub async fn get_eval_quality_snapshot(
    State(state): State<Arc<AppState>>,
    Path(snapshot_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    match store.get_quality_snapshot(&snapshot_id).await {
        Ok(Some(snapshot)) => (
            StatusCode::OK,
            Json(json!({ "quality_snapshot": snapshot })),
        ),
        Ok(None) => not_found("quality snapshot not found"),
        Err(error) => internal_error("failed to get quality snapshot", error),
    }
}

pub async fn list_eval_quality_snapshots_for_pr(
    State(state): State<Arc<AppState>>,
    Path((owner, repo, pr_number)): Path<(String, String, u64)>,
    Query(query): Query<EvalListQuery>,
) -> (StatusCode, Json<Value>) {
    let store = match eval_store(&state).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let repo_slug = format!("{owner}/{repo}");
    match store
        .list_quality_snapshots_for_pr(&repo_slug, pr_number, query.limit.unwrap_or(50))
        .await
    {
        Ok(snapshots) => (
            StatusCode::OK,
            Json(json!({ "quality_snapshots": snapshots })),
        ),
        Err(error) => internal_error("failed to list quality snapshots", error),
    }
}

async fn eval_store(state: &Arc<AppState>) -> Result<Arc<EvalStore>, (StatusCode, Json<Value>)> {
    if let Some(status) = state
        .startup_statuses
        .iter()
        .find(|status| status.name == "eval_store" && !status.ready)
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "eval store unavailable",
                "message": status.error.as_deref().unwrap_or("eval store failed to initialize"),
            })),
        ));
    }
    #[cfg(not(test))]
    {
        state.core.eval_store.clone().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "eval store unavailable",
                    "message": "eval store is not initialized",
                })),
            )
        })
    }
    #[cfg(test)]
    {
        let data_dir = crate::http::init::expand_tilde(&state.core.server.config.server.data_dir);
        let db_path = harness_core::config::dirs::default_db_path(&data_dir, "evals");
        EvalStore::open_with_database_url(
            &db_path,
            state.core.server.config.server.database_url.as_deref(),
        )
        .await
        .map(Arc::new)
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "failed to open eval store",
                    "message": error.to_string(),
                })),
            )
        })
    }
}

async fn load_input_artifact(
    store: &EvalStore,
    run_id: &str,
) -> anyhow::Result<Option<PrRepairEvalInput>> {
    let artifacts = store.list_artifacts(run_id).await?;
    let Some(artifact) = artifacts
        .iter()
        .rev()
        .find(|artifact| artifact.artifact_type == "pr_repair_eval_input")
    else {
        return Ok(None);
    };
    let input = serde_json::from_str(&artifact.body)
        .with_context(|| "failed to parse pr_repair_eval_input artifact")?;
    Ok(Some(input))
}

fn is_request_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("eval input scenario and target")
        || message.contains("PR repair scoring requires a pull request target")
        || message.contains("failed to parse pr_repair_eval_input artifact")
        || message.contains("missing field")
        || message.contains("unknown variant")
}

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
}

fn not_found(message: &'static str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": message })))
}

fn internal_error(
    summary: &'static str,
    error: impl std::fmt::Display,
) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": summary,
            "message": error.to_string(),
        })),
    )
}
