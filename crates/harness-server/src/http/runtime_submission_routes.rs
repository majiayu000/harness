use super::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use harness_core::agent::ApprovalDecision;
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, Serialize)]
struct ApprovalResponse {
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct RuntimeSubmissionArtifact {
    task_id: String,
    turn: i64,
    artifact_type: String,
    content: String,
    created_at: String,
}

#[derive(Debug, Serialize)]
struct RuntimeSubmissionPrompt {
    task_id: String,
    turn: i64,
    phase: String,
    prompt: String,
    created_at: String,
}

pub(crate) async fn respond_to_approval(
    State(state): State<Arc<AppState>>,
    Path((turn_id, request_id)): Path<(String, String)>,
    Json(decision): Json<ApprovalDecision>,
) -> Response {
    respond_to_approval_with_manager(
        &state.core.server.thread_manager,
        turn_id,
        request_id,
        decision,
    )
    .await
}

pub(super) async fn respond_to_approval_with_manager(
    thread_manager: &crate::thread_manager::ThreadManager,
    turn_id: String,
    request_id: String,
    decision: ApprovalDecision,
) -> Response {
    let turn_id = turn_id.trim().to_string();
    let request_id = request_id.trim().to_string();
    if turn_id.is_empty() || request_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "turn_id and request_id must not be empty"})),
        )
            .into_response();
    }

    match thread_manager
        .respond_approval_on_runtime_handle(&turn_id, request_id, decision)
        .await
    {
        Ok(()) => Json(ApprovalResponse { accepted: true }).into_response(),
        Err(harness_core::error::HarnessError::TurnNotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "runtime turn not found"})),
        )
            .into_response(),
        Err(harness_core::error::HarnessError::Unsupported(message)) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "approval response unavailable", "message": message})),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(turn_id = %turn_id, "runtime approval response failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "runtime approval response failed"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn get_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.core.workflow_runtime_store.is_none() {
        return runtime_store_unavailable();
    }
    let task_id = harness_core::types::TaskId(id);
    match runtime_artifacts_by_handle(&state, &task_id).await {
        Ok(Some(artifacts)) => Json(artifacts).into_response(),
        Ok(None) => runtime_submission_not_found(),
        Err(error) => {
            tracing::error!("get_runtime_artifacts: runtime workflow lookup failed: {error}");
            internal_server_error()
        }
    }
}

pub(crate) async fn get_prompts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if state.core.workflow_runtime_store.is_none() {
        return runtime_store_unavailable();
    }
    let task_id = harness_core::types::TaskId(id);
    match runtime_prompts_by_handle(&state, &task_id).await {
        Ok(Some(prompts)) => Json(prompts).into_response(),
        Ok(None) => runtime_submission_not_found(),
        Err(error) => {
            tracing::error!("get_runtime_prompts: runtime workflow lookup failed: {error}");
            internal_server_error()
        }
    }
}

async fn runtime_workflow_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<harness_workflow::runtime::WorkflowInstance>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    let Some(workflow) = store
        .get_instance_by_submission_id(task_id.as_str())
        .await?
    else {
        return Ok(None);
    };
    let is_builtin = matches!(
        workflow.definition_id.as_str(),
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID
            | harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID
    );
    let is_declarative = workflow
        .data
        .get("definition_hash")
        .and_then(Value::as_str)
        .is_some_and(|hash| !hash.trim().is_empty());
    Ok((is_builtin || is_declarative).then_some(workflow))
}

fn runtime_submission_handle(
    workflow: &harness_workflow::runtime::WorkflowInstance,
    fallback: &harness_core::types::TaskId,
) -> String {
    crate::workflow_runtime_submission::runtime_issue_task_handle(workflow)
        .map(|task_id| task_id.0)
        .unwrap_or_else(|| fallback.as_str().to_string())
}

async fn runtime_artifacts_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<Vec<RuntimeSubmissionArtifact>>> {
    let Some(workflow) = runtime_workflow_by_handle(state, task_id).await? else {
        return Ok(None);
    };
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("runtime workflow lookup requires an available store");
    let commands = store.commands_for(&workflow.id).await?;
    let command_ids = commands
        .iter()
        .map(|command| command.id.clone())
        .collect::<Vec<_>>();
    let jobs_by_command = store.runtime_jobs_for_commands(&command_ids).await?;
    let submission_id = runtime_submission_handle(&workflow, task_id);
    let mut artifacts = Vec::new();
    append_runtime_artifacts_by_command_order(
        &mut artifacts,
        &submission_id,
        &command_ids,
        &jobs_by_command,
    )?;
    Ok(Some(artifacts))
}

fn append_runtime_artifacts_by_command_order(
    artifacts: &mut Vec<RuntimeSubmissionArtifact>,
    submission_id: &str,
    command_ids: &[String],
    jobs_by_command: &BTreeMap<String, Vec<harness_workflow::runtime::RuntimeJob>>,
) -> anyhow::Result<()> {
    for command_id in command_ids {
        let Some(jobs) = jobs_by_command.get(command_id) else {
            continue;
        };
        for job in jobs {
            let Some(output) = job.output.as_ref() else {
                continue;
            };
            let Some(raw_artifacts) = output.get("artifacts") else {
                continue;
            };
            let raw_artifacts = raw_artifacts.as_array().ok_or_else(|| {
                anyhow::anyhow!("runtime job {} artifacts field is not an array", job.id)
            })?;
            for raw_artifact in raw_artifacts {
                let artifact_type = raw_artifact
                    .get("artifact_type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!("runtime job {} artifact is missing artifact_type", job.id)
                    })?;
                let artifact = raw_artifact.get("artifact").cloned().unwrap_or(Value::Null);
                artifacts.push(RuntimeSubmissionArtifact {
                    task_id: submission_id.to_string(),
                    turn: artifacts.len() as i64,
                    artifact_type: artifact_type.to_string(),
                    content: serde_json::to_string_pretty(&artifact)?,
                    created_at: job.updated_at.to_rfc3339(),
                });
            }
        }
    }
    Ok(())
}

async fn runtime_prompts_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<Vec<RuntimeSubmissionPrompt>>> {
    let Some(workflow) = runtime_workflow_by_handle(state, task_id).await? else {
        return Ok(None);
    };
    let submission_id = runtime_submission_handle(&workflow, task_id);
    let prompt_ref = workflow.data.get("prompt_ref").and_then(Value::as_str);
    let prompt = match prompt_ref {
        Some(prompt_ref) => {
            crate::workflow_runtime_submission::lookup_prompt_submission_prompt_durable(
                state
                    .core
                    .workflow_runtime_store
                    .as_ref()
                    .expect("runtime workflow lookup requires an available store"),
                prompt_ref,
            )
            .await?
        }
        None => None,
    };
    Ok(Some(
        prompt
            .map(|prompt| {
                vec![RuntimeSubmissionPrompt {
                    task_id: submission_id,
                    turn: 0,
                    phase: "submit".to_string(),
                    prompt,
                    created_at: workflow.created_at.to_rfc3339(),
                }]
            })
            .unwrap_or_default(),
    ))
}

fn runtime_submission_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "runtime submission not found"})),
    )
        .into_response()
}

fn internal_server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal server error"})),
    )
        .into_response()
}

fn runtime_store_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "workflow runtime store unavailable"})),
    )
        .into_response()
}
