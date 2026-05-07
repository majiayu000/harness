use crate::http::AppState;
use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_pr_feedback_inspect_decision, ActivityArtifact, ActivityErrorKind, ActivityResult,
    DecisionValidator, PrFeedbackInspectDecisionInput, RuntimeJob, WorkflowDecisionRecord,
    WorkflowDefinition, WorkflowInstance, WorkflowSubject, PR_FEEDBACK_DEFINITION_ID,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

use super::data_helpers::{
    activity_name, child_workflow_artifact, dependency_task_ids_from_command,
    force_execute_from_project_policy, merge_child_issue_data, merge_json_object,
    merge_pr_feedback_child_data, optional_data_u64, optional_string, parse_issue_subject_key,
    parse_pr_subject_key, required_data_string, required_string, string_vec, PrFeedbackChildData,
};
use super::workspace::{is_active_pr_feedback_inspect_command, is_pr_feedback_inspect_command};

pub(super) async fn execute_start_child_workflow(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
) -> anyhow::Result<ActivityResult> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        anyhow::bail!("workflow runtime store is unavailable");
    };
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
    store.upsert_instance(&child).await?;
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

    let mut child_submission = None;
    if command
        .get("auto_submit")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let labels = string_vec(command, "labels");
        let force_execute = force_execute_from_project_policy(project_id, &labels);
        let task_id = TaskId::from_str(&format!(
            "repo-backlog:{}:issue:{issue_number}",
            repo.unwrap_or("<none>")
        ));
        let source = optional_string(command, "source").unwrap_or_else(|| "github".to_string());
        let external_id =
            optional_string(command, "external_id").unwrap_or_else(|| issue_number.to_string());
        let depends_on = dependency_task_ids_from_command(command, repo);
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
            },
        )
        .await?;
        child_submission = Some(submission);
        if let Some(updated) = store.get_instance(&child.id).await? {
            child = updated;
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

async fn execute_start_pr_feedback_child_workflow(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
    command: &Value,
    subject_key: &str,
) -> anyhow::Result<ActivityResult> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        anyhow::bail!("workflow runtime store is unavailable");
    };
    let parent =
        parent.ok_or_else(|| anyhow::anyhow!("pr_feedback child workflow requires a parent"))?;
    let pr_number = parse_pr_subject_key(subject_key)
        .or_else(|| command.get("pr_number").and_then(Value::as_u64))
        .ok_or_else(|| anyhow::anyhow!("pr_feedback child workflow pr_number is missing"))?;
    let project_id = parent
        .data
        .get("project_id")
        .and_then(Value::as_str)
        .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("pr_feedback child workflow project_id is missing"))?;
    let repo = command
        .get("repo")
        .and_then(Value::as_str)
        .or_else(|| parent.data.get("repo").and_then(Value::as_str));
    let pr_url = command
        .get("pr_url")
        .and_then(Value::as_str)
        .or_else(|| parent.data.get("pr_url").and_then(Value::as_str));
    let issue_number = command
        .get("issue_number")
        .and_then(Value::as_u64)
        .or_else(|| parent.data.get("issue_number").and_then(Value::as_u64));
    let child_id = format!("{}::pr-feedback:{}", parent.id, job.command_id);
    store
        .upsert_definition(&WorkflowDefinition::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "PR feedback workflow",
        ))
        .await?;
    let mut child = match store.get_instance(&child_id).await? {
        Some(instance) => instance,
        None => WorkflowInstance::new(
            PR_FEEDBACK_DEFINITION_ID,
            1,
            "pending",
            WorkflowSubject::new("pr", subject_key),
        )
        .with_id(child_id.clone()),
    };
    if child.parent_workflow_id.is_none() {
        child.parent_workflow_id = Some(parent.id.clone());
    }
    child.data = merge_pr_feedback_child_data(
        child.data,
        PrFeedbackChildData {
            project_id,
            repo,
            issue_number,
            pr_number,
            pr_url,
            parent_workflow_id: parent.id.as_str(),
            runtime_job_id: job.id.as_str(),
            command_id: job.command_id.as_str(),
        },
    );
    store.upsert_instance(&child).await?;
    store
        .append_event(
            &child.id,
            "ChildWorkflowStarted",
            "workflow_runtime_worker",
            json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "definition_id": PR_FEEDBACK_DEFINITION_ID,
                "subject_key": subject_key,
            }),
        )
        .await?;

    let child_command_ids = if child.state == "pending" {
        let event = store
            .append_event(
                &child.id,
                "PrFeedbackInspectionRequested",
                "workflow_runtime_worker",
                json!({
                    "parent_workflow_id": parent.id.as_str(),
                    "pr_number": pr_number,
                    "pr_url": pr_url,
                    "issue_number": issue_number,
                    "repo": repo,
                }),
            )
            .await?;
        let stable_inspect_dedupe_key = format!("pr-feedback-child:{}:inspect", child.id);
        let existing_child_commands = store.commands_for(&child.id).await?;
        let active_inspect_command = existing_child_commands
            .iter()
            .find(|record| is_active_pr_feedback_inspect_command(record));
        let inspect_dedupe_key = match active_inspect_command {
            Some(record) => record.command.dedupe_key.clone(),
            None if existing_child_commands
                .iter()
                .any(is_pr_feedback_inspect_command) =>
            {
                format!("{}:retry:{}", stable_inspect_dedupe_key, event.id)
            }
            None => stable_inspect_dedupe_key,
        };
        let output = build_pr_feedback_inspect_decision(
            &child,
            PrFeedbackInspectDecisionInput {
                dedupe_key: &inspect_dedupe_key,
                pr_number,
                pr_url,
                issue_number,
                repo,
                parent_workflow_id: Some(parent.id.as_str()),
                summary: "PR feedback child workflow requested runtime inspection.",
            },
        );
        let validation = DecisionValidator::pr_feedback().validate(
            &child,
            &output.decision,
            &harness_workflow::runtime::ValidationContext::new(
                "workflow_runtime_worker",
                chrono::Utc::now(),
            ),
        );
        let record = match validation {
            Ok(()) => WorkflowDecisionRecord::accepted(output.decision.clone(), Some(event.id)),
            Err(error) => {
                let record = WorkflowDecisionRecord::rejected(
                    output.decision,
                    Some(event.id),
                    error.to_string(),
                );
                store.record_decision(&record).await?;
                return Ok(ActivityResult::failed(
                    activity_name(job),
                    "PR feedback child workflow inspection request was rejected.",
                    error.to_string(),
                )
                .with_error_kind(ActivityErrorKind::Configuration));
            }
        };
        store.record_decision(&record).await?;
        let mut command_ids = Vec::new();
        for command in &output.decision.commands {
            let command_id = store
                .enqueue_command(&child.id, Some(&record.id), command)
                .await?;
            let command_record = store
                .get_command(&command_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("workflow command `{command_id}` not found"))?;
            if !is_active_pr_feedback_inspect_command(&command_record) {
                anyhow::bail!(
                    "pr_feedback child inspect command `{command_id}` was not queued for dispatch"
                );
            }
            command_ids.push(command_id);
        }
        child.state = output.decision.next_state;
        child.version = child.version.saturating_add(1);
        store.upsert_instance(&child).await?;
        command_ids
    } else {
        Vec::new()
    };

    Ok(ActivityResult::succeeded(
        activity_name(job),
        format!("PR feedback child workflow `{}` started.", child.id),
    )
    .with_artifact(ActivityArtifact::new(
        "child_workflow",
        json!({
            "workflow_id": child.id,
            "definition_id": child.definition_id,
            "state": child.state,
            "subject_key": child.subject.subject_key,
        }),
    ))
    .with_artifact(ActivityArtifact::new(
        "child_commands",
        json!({
            "command_ids": child_command_ids,
        }),
    )))
}

pub(super) async fn execute_mark_bound_issue_done(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
) -> anyhow::Result<ActivityResult> {
    let Some(parent) = parent else {
        anyhow::bail!("mark_bound_issue_done parent workflow is missing");
    };
    let Some(issue_number) = optional_data_u64(parent, "last_issue_number") else {
        return Ok(ActivityResult::succeeded(
            activity_name(job),
            "No bound issue workflow was available to mark done.",
        ));
    };
    let child = upsert_child_issue_state(
        state,
        job,
        parent,
        issue_number,
        "done",
        "BoundIssueMarkedDone",
        json!({
            "pr_number": optional_data_u64(parent, "last_pr_number"),
            "pr_url": parent.data.get("last_pr_url").and_then(Value::as_str),
        }),
    )
    .await?;
    Ok(ActivityResult::succeeded(
        activity_name(job),
        format!("Bound issue workflow `{}` marked done.", child.id),
    )
    .with_artifact(ActivityArtifact::new(
        "child_workflow",
        child_workflow_artifact(&child),
    )))
}

pub(super) async fn execute_recover_issue_workflow(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
) -> anyhow::Result<ActivityResult> {
    let Some(parent) = parent else {
        anyhow::bail!("recover_issue_workflow parent workflow is missing");
    };
    let Some(issue_number) = optional_data_u64(parent, "last_issue_number") else {
        return Ok(ActivityResult::succeeded(
            activity_name(job),
            "No stale issue workflow was available to recover.",
        ));
    };
    let child = upsert_child_issue_state(
        state,
        job,
        parent,
        issue_number,
        "scheduled",
        "IssueWorkflowRecovered",
        json!({
            "previous_state": parent.data.get("last_observed_state").and_then(Value::as_str),
            "previous_active_task_id": parent.data.get("last_active_task_id").and_then(Value::as_str),
            "recovery_reason": parent.data.get("last_recovery_reason").and_then(Value::as_str),
        }),
    )
    .await?;
    Ok(ActivityResult::succeeded(
        activity_name(job),
        format!("Issue workflow `{}` recovered to scheduled.", child.id),
    )
    .with_artifact(ActivityArtifact::new(
        "child_workflow",
        child_workflow_artifact(&child),
    )))
}

async fn upsert_child_issue_state(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: &WorkflowInstance,
    issue_number: u64,
    new_state: &str,
    event_type: &str,
    update: Value,
) -> anyhow::Result<WorkflowInstance> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        anyhow::bail!("workflow runtime store is unavailable");
    };
    let project_id = required_data_string(parent, "project_id")?;
    let repo = parent.data.get("repo").and_then(Value::as_str);
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
            new_state,
            WorkflowSubject::new("issue", format!("issue:{issue_number}")),
        )
        .with_id(child_id.clone()),
    };
    child.state = new_state.to_string();
    child.version = child.version.saturating_add(1);
    if child.parent_workflow_id.is_none() {
        child.parent_workflow_id = Some(parent.id.clone());
    }
    child.data = merge_child_issue_data(
        child.data,
        project_id,
        repo,
        issue_number,
        job.id.as_str(),
        job.command_id.as_str(),
    );
    merge_json_object(&mut child.data, update);
    store.upsert_instance(&child).await?;
    store
        .append_event(
            &child.id,
            event_type,
            "workflow_runtime_worker",
            json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "state": child.state,
                "issue_number": issue_number,
            }),
        )
        .await?;
    Ok(child)
}
