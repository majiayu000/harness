use crate::http::AppState;
use harness_workflow::runtime::{RuntimeJob, WorkflowInstance};
use serde_json::Value;
use std::path::PathBuf;

pub(super) async fn workflow_for_job(
    state: &AppState,
    job: &RuntimeJob,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    if let Some(workflow_id) = job.input.get("workflow_id").and_then(Value::as_str) {
        return store.get_instance(workflow_id).await;
    }
    let Some(command) = store.get_command(&job.command_id).await? else {
        return Ok(None);
    };
    store.get_instance(&command.workflow_id).await
}

pub(super) fn project_root_for_job(
    state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<PathBuf> {
    if let Some(project_id) = workflow
        .and_then(|workflow| workflow.data.get("project_id"))
        .and_then(Value::as_str)
        .or_else(|| job.input.get("project_id").and_then(Value::as_str))
    {
        let project_root = PathBuf::from(project_id);
        if project_root.exists() {
            return Ok(project_root);
        }
        anyhow::bail!(
            "workflow project_id path is not resolvable: {}",
            project_root.display()
        );
    }
    Ok(state.core.project_root.clone())
}
