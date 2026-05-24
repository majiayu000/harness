use axum::{
    body::{to_bytes, Body},
    extract::{rejection::QueryRejection, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

use super::{state::AppState, task_query_routes::RawTaskListParams};
use crate::task_db::{TaskArtifact, TaskPrompt};

const RESPONSE_BODY_LIMIT: usize = 10 * 1024 * 1024;

pub(crate) async fn list_tasks(
    State(state): State<Arc<AppState>>,
    query: Result<Query<RawTaskListParams>, QueryRejection>,
) -> Response {
    let response = super::task_query_routes::list_tasks(State(state), query).await;
    decorate_json_response(response, decorate_task_list_response, "list_tasks").await
}

pub(crate) async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let response = super::task_query_routes::get_task(State(state), Path(id)).await;
    decorate_json_response(response, decorate_runtime_task_value, "get_task").await
}

pub(crate) async fn get_task_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match runtime_artifacts_by_handle(&state, &task_id).await {
        Ok(Some(artifacts)) => return Json(artifacts).into_response(),
        Ok(None) => {}
        Err(error) => {
            tracing::error!("get_task_artifacts: runtime workflow lookup failed: {error}");
            return internal_server_error();
        }
    }

    super::task_query_routes::get_task_artifacts(State(state), Path(task_id.0)).await
}

pub(crate) async fn get_task_prompts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match runtime_prompts_by_handle(&state, &task_id).await {
        Ok(Some(prompts)) => return Json(prompts).into_response(),
        Ok(None) => {}
        Err(error) => {
            tracing::error!("get_task_prompts: runtime workflow lookup failed: {error}");
            return internal_server_error();
        }
    }

    super::task_query_routes::get_task_prompts(State(state), Path(task_id.0)).await
}

async fn decorate_json_response(
    response: Response,
    decorate: fn(&mut Value),
    handler_name: &'static str,
) -> Response {
    let (mut parts, body) = response.into_parts();
    let bytes = match to_bytes(body, RESPONSE_BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!("{handler_name}: failed to read response body: {error}");
            return internal_server_error();
        }
    };

    if !parts.status.is_success() {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let mut value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!("{handler_name}: failed to decode response JSON: {error}");
            return internal_server_error();
        }
    };
    decorate(&mut value);

    let body = match serde_json::to_vec(&value) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!("{handler_name}: failed to encode response JSON: {error}");
            return internal_server_error();
        }
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}

fn decorate_task_list_response(value: &mut Value) {
    let Some(tasks) = value.get_mut("data").and_then(Value::as_array_mut) else {
        return;
    };
    for task in tasks {
        decorate_runtime_task_value(task);
    }
}

fn decorate_runtime_task_value(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !is_runtime_submission_object(object) {
        return;
    }

    let Some(submission_id) = object
        .get("submission_id")
        .and_then(Value::as_str)
        .or_else(|| object.get("task_id").and_then(Value::as_str))
        .or_else(|| object.get("id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
    else {
        return;
    };

    object.insert("submission_id".to_string(), json!(submission_id.clone()));
    object
        .entry("task_id".to_string())
        .or_insert_with(|| json!(submission_id));

    let workflow_id = object
        .get("workflow_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            object
                .get("workflow")
                .and_then(|workflow| workflow.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    if let Some(workflow_id) = workflow_id {
        object.insert("workflow_id".to_string(), json!(workflow_id));
    }
    object.insert("execution_path".to_string(), json!("workflow_runtime"));
}

fn is_runtime_submission_object(object: &serde_json::Map<String, Value>) -> bool {
    if object
        .get("execution_path")
        .and_then(Value::as_str)
        .is_some_and(|path| path == "workflow_runtime")
    {
        return true;
    }

    object
        .get("workflow")
        .and_then(|workflow| workflow.get("definition_id"))
        .and_then(Value::as_str)
        .is_some_and(is_runtime_submission_definition)
}

fn is_runtime_submission_definition(definition_id: &str) -> bool {
    matches!(
        definition_id,
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID
            | harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID
    )
}

async fn runtime_workflow_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<harness_workflow::runtime::WorkflowInstance>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    let Some(workflow) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(None);
    };
    if is_runtime_submission_definition(&workflow.definition_id) {
        Ok(Some(workflow))
    } else {
        Ok(None)
    }
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
) -> anyhow::Result<Option<Vec<TaskArtifact>>> {
    let Some(workflow) = runtime_workflow_by_handle(state, task_id).await? else {
        return Ok(None);
    };
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };

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
    artifacts: &mut Vec<TaskArtifact>,
    submission_id: &str,
    command_ids: &[String],
    jobs_by_command: &BTreeMap<String, Vec<harness_workflow::runtime::RuntimeJob>>,
) -> anyhow::Result<()> {
    for command_id in command_ids {
        let Some(jobs) = jobs_by_command.get(command_id) else {
            continue;
        };
        for job in jobs {
            append_runtime_job_artifacts(artifacts, submission_id, job)?;
        }
    }
    Ok(())
}

fn append_runtime_job_artifacts(
    artifacts: &mut Vec<TaskArtifact>,
    submission_id: &str,
    job: &harness_workflow::runtime::RuntimeJob,
) -> anyhow::Result<()> {
    let Some(output) = job.output.as_ref() else {
        return Ok(());
    };
    let Some(raw_artifacts) = output.get("artifacts") else {
        return Ok(());
    };
    let raw_artifacts = raw_artifacts.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "runtime job {} artifacts field is not an array",
            job.id.as_str()
        )
    })?;
    for raw_artifact in raw_artifacts {
        let artifact_type = raw_artifact
            .get("artifact_type")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("runtime job {} artifact is missing artifact_type", job.id)
            })?;
        let artifact = raw_artifact.get("artifact").cloned().unwrap_or(Value::Null);
        artifacts.push(TaskArtifact {
            task_id: submission_id.to_string(),
            turn: artifacts.len() as i64,
            artifact_type: artifact_type.to_string(),
            content: serde_json::to_string_pretty(&artifact)?,
            created_at: job.updated_at.to_rfc3339(),
        });
    }
    Ok(())
}

async fn runtime_prompts_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<Vec<TaskPrompt>>> {
    let Some(workflow) = runtime_workflow_by_handle(state, task_id).await? else {
        return Ok(None);
    };
    let submission_id = runtime_submission_handle(&workflow, task_id);
    let prompt = workflow
        .data
        .get("prompt_ref")
        .and_then(Value::as_str)
        .and_then(crate::workflow_runtime_submission::lookup_prompt_submission_prompt);

    let prompts = prompt
        .map(|prompt| {
            vec![TaskPrompt {
                task_id: submission_id,
                turn: 0,
                phase: "submit".to_string(),
                prompt,
                created_at: workflow.created_at.to_rfc3339(),
            }]
        })
        .unwrap_or_default();
    Ok(Some(prompts))
}

fn internal_server_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal server error"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::Request, routing::get, Router};
    use chrono::Utc;
    use harness_workflow::runtime::{
        ActivityArtifact, ActivityResult, RuntimeJob, RuntimeKind, WorkflowCommand,
        WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
    };
    use tower::ServiceExt;

    #[test]
    fn decorate_runtime_detail_exposes_submission_handle() {
        let mut value = json!({
            "id": "runtime-handle",
            "task_kind": "issue",
            "status": "implementing",
            "execution_path": "workflow_runtime",
            "workflow_id": "workflow-1"
        });

        decorate_runtime_task_value(&mut value);

        assert_eq!(value["task_id"], "runtime-handle");
        assert_eq!(value["submission_id"], "runtime-handle");
        assert_eq!(value["workflow_id"], "workflow-1");
        assert_eq!(value["execution_path"], "workflow_runtime");
    }

    #[test]
    fn decorate_runtime_list_row_uses_workflow_summary() {
        let mut value = json!({
            "id": "runtime-list-handle",
            "task_kind": "prompt",
            "status": "implementing",
            "workflow": {
                "id": "workflow-2",
                "definition_id": "prompt_task",
                "state": "implementing"
            }
        });

        decorate_runtime_task_value(&mut value);

        assert_eq!(value["task_id"], "runtime-list-handle");
        assert_eq!(value["submission_id"], "runtime-list-handle");
        assert_eq!(value["workflow_id"], "workflow-2");
        assert_eq!(value["execution_path"], "workflow_runtime");
    }

    #[test]
    fn runtime_submission_handle_prefers_first_recorded_task_id() {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:1128"),
        )
        .with_data(json!({
            "task_id": "latest-handle",
            "task_ids": ["first-handle", "latest-handle"]
        }));
        let fallback = harness_core::types::TaskId::from_str("fallback-handle");

        assert_eq!(
            runtime_submission_handle(&workflow, &fallback),
            "first-handle"
        );
    }

    #[test]
    fn runtime_job_artifacts_keep_submission_handle() -> anyhow::Result<()> {
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new("pull_request", json!({"number": 1157})),
        );
        let mut job =
            RuntimeJob::pending("command-1", RuntimeKind::CodexExec, "default", json!({}));
        job.complete(&result)?;
        let mut artifacts = Vec::new();

        append_runtime_job_artifacts(&mut artifacts, "runtime-handle", &job)?;

        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].task_id, "runtime-handle");
        assert_eq!(artifacts[0].artifact_type, "pull_request");
        assert!(artifacts[0].content.contains("\"number\": 1157"));
        Ok(())
    }

    #[test]
    fn runtime_job_artifacts_follow_workflow_command_order() -> anyhow::Result<()> {
        let mut first_command_job =
            RuntimeJob::pending("command-b", RuntimeKind::CodexExec, "default", json!({}));
        let first_result = ActivityResult::succeeded("implement_issue", "first").with_artifact(
            ActivityArtifact::new("pull_request", json!({"source": "command-b"})),
        );
        first_command_job.complete(&first_result)?;

        let mut second_command_job =
            RuntimeJob::pending("command-a", RuntimeKind::CodexExec, "default", json!({}));
        let second_result = ActivityResult::succeeded("implement_issue", "second").with_artifact(
            ActivityArtifact::new("pull_request", json!({"source": "command-a"})),
        );
        second_command_job.complete(&second_result)?;

        let command_ids = vec!["command-b".to_string(), "command-a".to_string()];
        let mut jobs_by_command = std::collections::BTreeMap::new();
        jobs_by_command.insert("command-a".to_string(), vec![second_command_job]);
        jobs_by_command.insert("command-b".to_string(), vec![first_command_job]);
        let mut artifacts = Vec::new();

        append_runtime_artifacts_by_command_order(
            &mut artifacts,
            "runtime-handle",
            &command_ids,
            &jobs_by_command,
        )?;

        assert_eq!(artifacts.len(), 2);
        assert_eq!(artifacts[0].turn, 0);
        assert!(artifacts[0].content.contains("\"source\": \"command-b\""));
        assert_eq!(artifacts[1].turn, 1);
        assert!(artifacts[1].content.contains("\"source\": \"command-a\""));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_routes_do_not_require_legacy_task_row() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let mut state = crate::test_helpers::make_test_state(dir.path()).await?;
        let store = Arc::new(
            WorkflowRuntimeStore::open_with_database_url(
                &harness_core::config::dirs::default_db_path(dir.path(), "workflow_runtime"),
                Some(&crate::test_helpers::test_database_url()?),
            )
            .await?,
        );
        state.core.workflow_runtime_store = Some(store.clone());
        let state = Arc::new(state);

        let task_id = harness_core::types::TaskId::from_str("runtime-route-handle");
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:1128"),
        )
        .with_id("runtime-route-workflow")
        .with_data(json!({
            "task_id": task_id.as_str(),
            "task_ids": [task_id.as_str()],
            "issue_number": 1128,
            "repo": "owner/repo",
            "project_id": "/tmp/project"
        }));
        store.upsert_instance(&workflow).await?;

        let command_id = store
            .enqueue_command(
                &workflow.id,
                None,
                &WorkflowCommand::enqueue_activity("implement_issue", "route-test"),
            )
            .await?;
        store
            .enqueue_runtime_job(&command_id, RuntimeKind::CodexExec, "default", json!({}))
            .await?;
        let owner = "route-test-owner";
        let claimed = store
            .claim_next_runtime_job(owner, Utc::now() + chrono::Duration::minutes(5))
            .await?
            .ok_or_else(|| anyhow::anyhow!("runtime job should be claimable"))?;
        let lease_expires_at = claimed
            .lease
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("claimed job should have a lease"))?
            .expires_at;
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new("pull_request", json!({"number": 1158})),
        );
        store
            .complete_runtime_job_if_owned(&claimed.id, owner, lease_expires_at, &result)
            .await?
            .ok_or_else(|| anyhow::anyhow!("runtime job should complete"))?;

        assert!(state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none());

        let app = Router::new()
            .route("/tasks", get(list_tasks))
            .route("/tasks/{id}", get(get_task))
            .route("/tasks/{id}/artifacts", get(get_task_artifacts))
            .route("/tasks/{id}/prompts", get(get_task_prompts))
            .with_state(state);

        let detail = get_json(&app, "/tasks/runtime-route-handle").await?;
        assert_eq!(detail["id"], "runtime-route-handle");
        assert_eq!(detail["task_id"], "runtime-route-handle");
        assert_eq!(detail["submission_id"], "runtime-route-handle");
        assert_eq!(detail["workflow_id"], "runtime-route-workflow");

        let artifacts = get_json(&app, "/tasks/runtime-route-handle/artifacts").await?;
        assert_eq!(artifacts[0]["task_id"], "runtime-route-handle");
        assert_eq!(artifacts[0]["artifact_type"], "pull_request");

        let prompts = get_json(&app, "/tasks/runtime-route-handle/prompts").await?;
        assert_eq!(
            prompts
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("prompts should be an array"))?
                .len(),
            0
        );

        let list = get_json(&app, "/tasks").await?;
        let row = list["data"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("task list data should be an array"))?
            .iter()
            .find(|task| task["id"] == "runtime-route-handle")
            .ok_or_else(|| anyhow::anyhow!("runtime row should be listed"))?;
        assert_eq!(row["task_id"], "runtime-route-handle");
        assert_eq!(row["submission_id"], "runtime-route-handle");
        assert_eq!(row["workflow_id"], "runtime-route-workflow");
        assert_eq!(row["execution_path"], "workflow_runtime");

        Ok(())
    }

    async fn get_json(app: &Router, uri: &str) -> anyhow::Result<Value> {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty())?)
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}
