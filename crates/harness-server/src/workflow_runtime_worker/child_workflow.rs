use crate::http::AppState;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, RuntimeJob, WorkflowDefinition, WorkflowInstance,
    WorkflowSubject, PROMPT_TASK_DEFINITION_ID, PR_FEEDBACK_DEFINITION_ID,
    QUALITY_GATE_DEFINITION_ID,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

use super::child_workflow_non_issue::{
    execute_start_pr_feedback_child_workflow, execute_start_prompt_task_child_workflow,
    execute_start_quality_gate_child_workflow,
};
use super::child_workflow_replay::{
    child_start_event_recorded, child_started_by_command, ensure_runtime_job_still_owns_lease,
    issue_submission_recorded,
};
use super::data_helpers::{
    activity_name, dependency_task_ids_from_command, force_execute_from_project_policy,
    issue_task_id_from_command, issue_task_prefix_from_task_id, merge_child_issue_data,
    optional_string, parse_issue_subject_key, required_string, string_vec,
};

pub(super) async fn execute_start_child_workflow(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
) -> anyhow::Result<ActivityResult> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        anyhow::bail!("workflow runtime store is unavailable");
    };
    ensure_runtime_job_still_owns_lease(store, job).await?;
    let command = job
        .input
        .get("command")
        .ok_or_else(|| anyhow::anyhow!("start_child_workflow command payload is missing"))?;
    let definition_id = required_string(command, "definition_id")?;
    let subject_key = required_string(command, "subject_key")?;
    if definition_id == PR_FEEDBACK_DEFINITION_ID {
        return execute_start_pr_feedback_child_workflow(state, job, parent, command, subject_key)
            .await;
    }
    if definition_id == PROMPT_TASK_DEFINITION_ID {
        return execute_start_prompt_task_child_workflow(state, job, parent, command, subject_key)
            .await;
    }
    if definition_id == QUALITY_GATE_DEFINITION_ID {
        return execute_start_quality_gate_child_workflow(state, job, parent, command, subject_key)
            .await;
    }
    if definition_id != "github_issue_pr" {
        anyhow::bail!("start_child_workflow definition `{definition_id}` is not supported yet");
    }
    let issue_number = parse_issue_subject_key(subject_key)?;
    let project_id = parent
        .and_then(|workflow| workflow.data.get("project_id"))
        .and_then(Value::as_str)
        .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("start_child_workflow project_id is missing"))?;
    let repo = parent
        .and_then(|workflow| workflow.data.get("repo"))
        .and_then(Value::as_str)
        .or_else(|| command.get("repo").and_then(Value::as_str));
    let child_id = harness_workflow::issue_lifecycle::workflow_id(project_id, repo, issue_number);
    store
        .upsert_definition(&WorkflowDefinition::new(
            "github_issue_pr",
            1,
            "GitHub issue PR workflow",
        ))
        .await?;
    let mut child = match store.get_instance(&child_id).await? {
        Some(instance) => instance,
        None => WorkflowInstance::new(
            "github_issue_pr",
            1,
            "discovered",
            WorkflowSubject::new("issue", subject_key),
        )
        .with_id(child_id.clone()),
    };
    let child_started_by_command = child_started_by_command(&child, &job.command_id);
    let child_start_event_recorded =
        child_start_event_recorded(store, &child.id, &job.command_id).await?;
    if child.parent_workflow_id.is_none() {
        if let Some(parent) = parent {
            child.parent_workflow_id = Some(parent.id.clone());
        }
    }
    child.data = merge_child_issue_data(
        child.data,
        project_id,
        repo,
        issue_number,
        job.id.as_str(),
        job.command_id.as_str(),
    );
    if !child_started_by_command || !child_start_event_recorded {
        store.upsert_instance(&child).await?;
        if !child_start_event_recorded {
            store
                .append_event(
                    &child.id,
                    "ChildWorkflowStarted",
                    "workflow_runtime_worker",
                    json!({
                        "parent_workflow_id": parent.map(|workflow| workflow.id.as_str()),
                        "runtime_job_id": job.id.as_str(),
                        "command_id": job.command_id.as_str(),
                        "definition_id": definition_id,
                        "subject_key": subject_key,
                    }),
                )
                .await?;
        }
    }

    let mut child_submission = None;
    if command
        .get("auto_submit")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let labels = string_vec(command, "labels");
        let force_execute = force_execute_from_project_policy(project_id, &labels);
        let task_id = issue_task_id_from_command(command, job, repo, issue_number);
        let issue_task_prefix = issue_task_prefix_from_task_id(&task_id, issue_number);
        if !issue_submission_recorded(store, &child, &task_id).await? {
            let source = optional_string(command, "source").unwrap_or_else(|| "github".to_string());
            let external_id =
                optional_string(command, "external_id").unwrap_or_else(|| issue_number.to_string());
            let depends_on =
                dependency_task_ids_from_command(command, repo, issue_task_prefix.as_deref());
            let submission = crate::workflow_runtime_submission::record_issue_submission(
                store,
                crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
                    project_root: Path::new(project_id),
                    repo,
                    issue_number,
                    task_id: &task_id,
                    labels: &labels,
                    force_execute,
                    additional_prompt: None,
                    depends_on: &depends_on,
                    dependencies_blocked: !depends_on.is_empty(),
                    source: Some(source.as_str()),
                    external_id: Some(external_id.as_str()),
                    remote_fact_hash: None,
                },
            )
            .await?;
            child_submission = Some(submission);
            if let Some(updated) = store.get_instance(&child.id).await? {
                child = updated;
            }
        }
    }

    let mut result = ActivityResult::succeeded(
        activity_name(job),
        format!("Child workflow `{}` started.", child.id),
    )
    .with_artifact(ActivityArtifact::new(
        "child_workflow",
        json!({
            "workflow_id": child.id,
            "definition_id": child.definition_id,
            "state": child.state,
            "subject_key": child.subject.subject_key,
        }),
    ));
    if let Some(submission) = child_submission {
        result = result.with_artifact(ActivityArtifact::new(
            "child_submission",
            json!({
                "workflow_id": submission.workflow_id,
                "accepted": submission.accepted,
                "decision_id": submission.decision_id,
                "command_ids": submission.command_ids,
                "rejection_reason": submission.rejection_reason,
            }),
        ));
    }
    Ok(result)
}
