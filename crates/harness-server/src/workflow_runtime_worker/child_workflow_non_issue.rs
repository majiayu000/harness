use crate::http::AppState;
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    build_pr_feedback_inspect_decision, build_quality_gate_run_decision, ActivityArtifact,
    ActivityErrorKind, ActivityResult, DecisionValidator, PrFeedbackInspectDecisionInput,
    QualityGateDecisionInput, RuntimeJob, WorkflowDecisionRecord, WorkflowDefinition,
    WorkflowInstance, WorkflowSubject, PROMPT_TASK_DEFINITION_ID, PR_FEEDBACK_DEFINITION_ID,
    QUALITY_GATE_DEFINITION_ID,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

use super::child_workflow_replay::{
    child_event_id_or_append, child_start_event_recorded, child_started_by_command,
    decision_for_event, ensure_runtime_job_still_owns_lease,
};
use super::data_helpers::{
    activity_name, merge_json_object, merge_pr_feedback_child_data, optional_string,
    parse_pr_subject_key, required_string, string_vec, PrFeedbackChildData,
};
use super::workspace::{is_active_pr_feedback_inspect_command, is_pr_feedback_inspect_command};

pub(super) async fn execute_start_prompt_task_child_workflow(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
    command: &Value,
    subject_key: &str,
) -> anyhow::Result<ActivityResult> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        anyhow::bail!("workflow runtime store is unavailable");
    };
    ensure_runtime_job_still_owns_lease(store, job).await?;
    let project_id = parent
        .and_then(|workflow| workflow.data.get("project_id"))
        .and_then(Value::as_str)
        .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("prompt task child workflow project_id is missing"))?;
    let prompt = required_string(command, "prompt")?;
    let repo = command.get("repo").and_then(Value::as_str).or_else(|| {
        parent
            .and_then(|workflow| workflow.data.get("repo"))
            .and_then(Value::as_str)
    });
    let task_id = optional_string(command, "task_id").unwrap_or_else(|| {
        format!(
            "runtime-child:{}:{}",
            repo.unwrap_or("<none>"),
            subject_key.replace(':', "-")
        )
    });
    let task_id = TaskId::from_str(&task_id);
    let source = optional_string(command, "source").unwrap_or_else(|| "runtime_child".to_string());
    let external_id =
        optional_string(command, "external_id").unwrap_or_else(|| subject_key.to_string());
    let submission = crate::workflow_runtime_submission::record_prompt_submission(
        store,
        crate::workflow_runtime_submission::PromptSubmissionRuntimeContext {
            project_root: Path::new(project_id),
            task_id: &task_id,
            prompt,
            depends_on: &[],
            serialization_depends_on: &[],
            dependencies_blocked: false,
            source: Some(source.as_str()),
            external_id: Some(external_id.as_str()),
            continuation: None,
        },
    )
    .await?;

    let mut child = store
        .get_instance(&submission.workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("prompt task child workflow was not persisted"))?;
    let child_start_event_recorded =
        child_start_event_recorded(store, &child.id, &job.command_id).await?;

    if let Some(parent) = parent {
        if child.parent_workflow_id.is_none() {
            child.parent_workflow_id = Some(parent.id.clone());
            store.upsert_instance(&child).await?;
        }
    }

    if !child_start_event_recorded {
        store
            .append_event(
                &submission.workflow_id,
                "ChildWorkflowStarted",
                "workflow_runtime_worker",
                json!({
                    "parent_workflow_id": parent.map(|workflow| workflow.id.as_str()),
                    "runtime_job_id": job.id.as_str(),
                    "command_id": job.command_id.as_str(),
                    "definition_id": PROMPT_TASK_DEFINITION_ID,
                    "subject_key": subject_key,
                }),
            )
            .await?;
    }

    Ok(ActivityResult::succeeded(
        activity_name(job),
        format!("Prompt task child workflow `{}` started.", child.id),
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
        "child_submission",
        json!({
            "workflow_id": submission.workflow_id,
            "accepted": submission.accepted,
            "decision_id": submission.decision_id,
            "command_ids": submission.command_ids,
            "rejection_reason": submission.rejection_reason,
        }),
    )))
}

pub(super) async fn execute_start_quality_gate_child_workflow(
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
        parent.ok_or_else(|| anyhow::anyhow!("quality_gate child workflow requires a parent"))?;
    let project_id = parent
        .data
        .get("project_id")
        .and_then(Value::as_str)
        .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("quality_gate child workflow project_id is missing"))?;
    let repo = command
        .get("repo")
        .and_then(Value::as_str)
        .or_else(|| parent.data.get("repo").and_then(Value::as_str));
    let pr_number = command
        .get("pr_number")
        .and_then(Value::as_u64)
        .or_else(|| parent.data.get("pr_number").and_then(Value::as_u64));
    let pr_url = optional_string(command, "pr_url").or_else(|| {
        parent
            .data
            .get("pr_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    let validation_commands = string_vec(command, "validation_commands");
    let child_id = format!("{}::quality-gate:{}", parent.id, job.command_id);
    store
        .upsert_definition(&WorkflowDefinition::new(
            QUALITY_GATE_DEFINITION_ID,
            1,
            "Quality gate workflow",
        ))
        .await?;
    let mut child = match store.get_instance(&child_id).await? {
        Some(instance) => instance,
        None => WorkflowInstance::new(
            QUALITY_GATE_DEFINITION_ID,
            1,
            "pending",
            WorkflowSubject::new("quality_gate", subject_key),
        )
        .with_id(child_id.clone()),
    };
    let child_started_by_command = child_started_by_command(&child, &job.command_id);
    let child_start_event_recorded =
        child_start_event_recorded(store, &child.id, &job.command_id).await?;
    if child.parent_workflow_id.is_none() {
        child.parent_workflow_id = Some(parent.id.clone());
    }
    merge_json_object(
        &mut child.data,
        json!({
            "project_id": project_id,
            "repo": repo,
            "pr_number": pr_number,
            "pr_url": pr_url.clone(),
            "parent_workflow_id": parent.id.as_str(),
            "runtime_job_id": job.id.as_str(),
            "command_id": job.command_id.as_str(),
            "started_by_runtime_job_id": job.id.as_str(),
            "started_by_command_id": job.command_id.as_str(),
            "validation_commands": validation_commands.clone(),
        }),
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
                        "parent_workflow_id": parent.id.as_str(),
                        "runtime_job_id": job.id.as_str(),
                        "command_id": job.command_id.as_str(),
                        "definition_id": QUALITY_GATE_DEFINITION_ID,
                        "subject_key": subject_key,
                    }),
                )
                .await?;
        }
    }

    let child_command_ids = if child.state == "pending" {
        let event_id = child_event_id_or_append(
            store,
            &child.id,
            "QualityGateRequested",
            json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "pr_number": pr_number,
                "pr_url": pr_url.clone(),
                "repo": repo,
            }),
        )
        .await?;
        let existing_record = decision_for_event(store, &child.id, &event_id).await?;
        let output = build_quality_gate_run_decision(
            &child,
            QualityGateDecisionInput {
                reason: "Parent PR workflow requested a quality gate before ready_to_merge.",
                validation_commands: &validation_commands,
            },
        );
        let validation = DecisionValidator::quality_gate().validate(
            &child,
            &output.decision,
            &harness_workflow::runtime::ValidationContext::new(
                "workflow_runtime_worker",
                chrono::Utc::now(),
            ),
        );
        let record = if let Some(record) = existing_record {
            record
        } else {
            match validation {
                Ok(()) => WorkflowDecisionRecord::accepted(output.decision.clone(), Some(event_id)),
                Err(error) => {
                    let record = WorkflowDecisionRecord::rejected(
                        output.decision,
                        Some(event_id),
                        error.to_string(),
                    );
                    store.record_decision(&record).await?;
                    return Ok(ActivityResult::failed(
                        activity_name(job),
                        "Quality gate child workflow request was rejected.",
                        error.to_string(),
                    )
                    .with_error_kind(ActivityErrorKind::Configuration));
                }
            }
        };
        if !record.accepted {
            return Ok(ActivityResult::failed(
                activity_name(job),
                "Quality gate child workflow request was rejected.",
                record
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "decision rejected".to_string()),
            )
            .with_error_kind(ActivityErrorKind::Configuration));
        }
        store.record_decision(&record).await?;
        let mut command_ids = Vec::new();
        for command in &record.decision.commands {
            let command_id = store
                .enqueue_command(&child.id, Some(&record.id), command)
                .await?;
            command_ids.push(command_id);
        }
        child.state = record.decision.next_state.clone();
        child.version = child.version.saturating_add(1);
        store.upsert_instance(&child).await?;
        command_ids
    } else {
        Vec::new()
    };

    Ok(ActivityResult::succeeded(
        activity_name(job),
        format!("Quality gate child workflow `{}` started.", child.id),
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

pub(super) async fn execute_start_pr_feedback_child_workflow(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    parent: Option<&WorkflowInstance>,
    command: &Value,
    subject_key: &str,
) -> anyhow::Result<ActivityResult> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        anyhow::bail!("workflow runtime store is unavailable");
    };
    ensure_runtime_job_still_owns_lease(store, job).await?;
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
    let child_started_by_command = child_started_by_command(&child, &job.command_id);
    let child_start_event_recorded =
        child_start_event_recorded(store, &child.id, &job.command_id).await?;
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
    if !child_started_by_command || !child_start_event_recorded {
        store.upsert_instance(&child).await?;
        if !child_start_event_recorded {
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
        }
    }

    let child_command_ids = if child.state == "pending" {
        let event_id = child_event_id_or_append(
            store,
            &child.id,
            "PrFeedbackInspectionRequested",
            json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "pr_number": pr_number,
                "pr_url": pr_url,
                "issue_number": issue_number,
                "repo": repo,
            }),
        )
        .await?;
        let existing_record = decision_for_event(store, &child.id, &event_id).await?;
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
                format!("{}:retry:{}", stable_inspect_dedupe_key, event_id)
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
        let record = if let Some(record) = existing_record {
            record
        } else {
            match validation {
                Ok(()) => WorkflowDecisionRecord::accepted(output.decision.clone(), Some(event_id)),
                Err(error) => {
                    let record = WorkflowDecisionRecord::rejected(
                        output.decision,
                        Some(event_id),
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
            }
        };
        if !record.accepted {
            return Ok(ActivityResult::failed(
                activity_name(job),
                "PR feedback child workflow inspection request was rejected.",
                record
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "decision rejected".to_string()),
            )
            .with_error_kind(ActivityErrorKind::Configuration));
        }
        store.record_decision(&record).await?;
        let mut command_ids = Vec::new();
        for command in &record.decision.commands {
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
        child.state = record.decision.next_state.clone();
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
