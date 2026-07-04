use super::data_helpers::{optional_data_string, optional_string};
use super::turn_engine::helpers::RuntimeUsageContext;
use crate::http::AppState;
use harness_workflow::runtime::{RuntimeJob, RuntimeProfile, WorkflowInstance};
use serde_json::Value;
use std::path::Path;

pub(super) fn runtime_usage_context(
    state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    runtime_profile: &RuntimeProfile,
    agent_name: &str,
    source_project_root: &Path,
) -> Option<RuntimeUsageContext> {
    let store = state.core.workflow_runtime_store.as_ref()?.clone();
    let workflow_id = workflow
        .map(|workflow| workflow.id.clone())
        .or_else(|| optional_string(&job.input, "workflow_id"))
        .unwrap_or_else(|| "unknown".to_string());
    let candidate = job.input.pointer("/command/candidate");
    Some(RuntimeUsageContext {
        store,
        runtime_job_id: job.id.clone(),
        command_id: job.command_id.clone(),
        workflow_id,
        runtime_kind: job.runtime_kind,
        runtime_profile: job.runtime_profile.clone(),
        agent: agent_name.to_string(),
        model: runtime_profile
            .model
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        project: workflow
            .and_then(|workflow| optional_data_string(workflow, "project_id"))
            .unwrap_or_else(|| source_project_root.to_string_lossy().into_owned()),
        task_id: workflow
            .and_then(|workflow| optional_data_string(workflow, "task_id"))
            .or_else(|| optional_string(&job.input, "task_id")),
        candidate_group_id: candidate
            .and_then(|value| optional_string(value, "candidate_group_id")),
        candidate_id: candidate.and_then(|value| optional_string(value, "candidate_id")),
        candidate_index: candidate.and_then(|value| optional_u32_value(value, "candidate_index")),
        candidate_count: candidate.and_then(|value| optional_u32_value(value, "candidate_count")),
    })
}

fn optional_u32_value(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}
