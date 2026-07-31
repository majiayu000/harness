use crate::http::AppState;
use harness_core::types::TaskId;
use harness_workflow::runtime::{
    build_pr_feedback_inspect_decision, build_quality_gate_run_decision, ActivityArtifact,
    ActivityErrorKind, ActivityResult, PrFeedbackInspectDecisionInput, QualityGateDecisionInput,
    RuntimeJob, WorkflowChildStart, WorkflowCommandStatus, WorkflowDefinition, WorkflowInstance,
    WorkflowSubject, WorkflowSubmissionDecisionTransition, PROMPT_TASK_DEFINITION_ID,
    PR_FEEDBACK_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID,
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
    let parent =
        parent.ok_or_else(|| anyhow::anyhow!("prompt task child workflow requires a parent"))?;
    let project_id = parent
        .data
        .get("project_id")
        .and_then(Value::as_str)
        .or_else(|| job.input.get("project_id").and_then(Value::as_str))
        .ok_or_else(|| anyhow::anyhow!("prompt task child workflow project_id is missing"))?;
    let prompt = required_string(command, "prompt")?;
    let repo = command
        .get("repo")
        .and_then(Value::as_str)
        .or_else(|| parent.data.get("repo").and_then(Value::as_str));
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

    if child.parent_workflow_id.is_none() {
        child = store
            .attach_parent_workflow_if_missing(&child.id, &parent.id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("prompt task child workflow disappeared during parent attach")
            })?;
    }

    child = store
        .ensure_child_workflow_started(WorkflowChildStart {
            instance: &child,
            command_id: &job.command_id,
            source: "workflow_runtime_worker",
            payload: json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "definition_id": PROMPT_TASK_DEFINITION_ID,
                "subject_key": subject_key,
            }),
        })
        .await?
        .instance;

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
    let existing_child = store.get_instance(&child_id).await?;
    let child_was_persisted = existing_child.is_some();
    let mut child = match existing_child {
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
    if child.parent_workflow_id.is_none() && child_was_persisted {
        child = store
            .attach_parent_workflow_if_missing(&child.id, &parent.id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("quality gate child workflow disappeared during parent attach")
            })?;
    } else if child.parent_workflow_id.is_none() {
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
    let inherited_trust = inherit_author_trust_class(&mut child.data, &parent.data)?;
    if !child_started_by_command || !child_start_event_recorded {
        child = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &job.command_id,
                source: "workflow_runtime_worker",
                payload: json!({
                    "parent_workflow_id": parent.id.as_str(),
                    "runtime_job_id": job.id.as_str(),
                    "command_id": job.command_id.as_str(),
                    "definition_id": QUALITY_GATE_DEFINITION_ID,
                    "subject_key": subject_key,
                }),
            })
            .await?
            .instance;
    } else if let Some(inherited_trust) = inherited_trust {
        child = store
            .reconcile_instance_author_trust_class(&child.id, inherited_trust)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("quality gate child workflow disappeared during trust repair")
            })?;
    }

    let child_command_ids = if child.state == "pending" {
        let request_payload = json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "pr_number": pr_number,
                "pr_url": pr_url.clone(),
                "repo": repo,
        });
        let event_id = child_event_id_or_append(
            store,
            &child.id,
            "QualityGateRequested",
            request_payload.clone(),
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
        if let Some(record) = existing_record.as_ref().filter(|record| !record.accepted) {
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
        let decision = existing_record
            .as_ref()
            .map(|record| &record.decision)
            .unwrap_or(&output.decision);
        let mut final_child = child.clone();
        final_child.state = decision.next_state.clone();
        final_child.version = final_child.version.saturating_add(1);
        let commit = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &child.id,
                expected_state: &child.state,
                expected_version: child.version,
                create_if_missing: None,
                event_id: Some(&event_id),
                new_event_id: None,
                event_type: "QualityGateRequested",
                source: "workflow_runtime_worker",
                payload: request_payload,
                decision: &output.decision,
                existing_record: existing_record.as_ref(),
                rejection_reason: None,
                final_instance: Some(&final_child),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "quality gate child workflow state changed before request could be committed"
                )
            })?;
        if !commit.record.accepted {
            return Ok(ActivityResult::failed(
                activity_name(job),
                "Quality gate child workflow request was rejected.",
                commit
                    .record
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "decision rejected".to_string()),
            )
            .with_error_kind(ActivityErrorKind::Configuration));
        }
        child = final_child;
        commit.command_ids
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
    let remote_fact_hash = command.get("remote_fact_hash").and_then(Value::as_str);
    let remote_fact_activity_at = command
        .get("remote_fact_activity_at")
        .and_then(Value::as_str);
    let expected_base_ref = ["expected_base_ref", "target_base_ref", "base_ref"]
        .into_iter()
        .find_map(|field| {
            command
                .get(field)
                .and_then(Value::as_str)
                .or_else(|| parent.data.get(field).and_then(Value::as_str))
        });
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
    let existing_child = store.get_instance(&child_id).await?;
    let child_was_persisted = existing_child.is_some();
    let mut child = match existing_child {
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
    if child.parent_workflow_id.is_none() && child_was_persisted {
        child = store
            .attach_parent_workflow_if_missing(&child.id, &parent.id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("PR feedback child workflow disappeared during parent attach")
            })?;
    } else if child.parent_workflow_id.is_none() {
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
            expected_base_ref,
            parent_workflow_id: parent.id.as_str(),
            runtime_job_id: job.id.as_str(),
            command_id: job.command_id.as_str(),
            remote_fact_hash,
            remote_fact_activity_at,
        },
    );
    let inherited_trust = inherit_author_trust_class(&mut child.data, &parent.data)?;
    if !child_started_by_command || !child_start_event_recorded {
        child = store
            .ensure_child_workflow_started(WorkflowChildStart {
                instance: &child,
                command_id: &job.command_id,
                source: "workflow_runtime_worker",
                payload: json!({
                    "parent_workflow_id": parent.id.as_str(),
                    "runtime_job_id": job.id.as_str(),
                    "command_id": job.command_id.as_str(),
                    "definition_id": PR_FEEDBACK_DEFINITION_ID,
                    "subject_key": subject_key,
                }),
            })
            .await?
            .instance;
    } else if let Some(inherited_trust) = inherited_trust {
        child = store
            .reconcile_instance_author_trust_class(&child.id, inherited_trust)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("PR feedback child workflow disappeared during trust repair")
            })?;
    }

    let child_command_ids = if child.state == "pending" {
        let request_payload = json!({
                "parent_workflow_id": parent.id.as_str(),
                "runtime_job_id": job.id.as_str(),
                "command_id": job.command_id.as_str(),
                "pr_number": pr_number,
                "pr_url": pr_url,
                "issue_number": issue_number,
                "repo": repo,
                "expected_base_ref": expected_base_ref,
        });
        let event_id = child_event_id_or_append(
            store,
            &child.id,
            "PrFeedbackInspectionRequested",
            request_payload.clone(),
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
                expected_base_ref,
                parent_workflow_id: Some(parent.id.as_str()),
                summary: "PR feedback child workflow requested runtime inspection.",
            },
        );
        if let Some(record) = existing_record.as_ref().filter(|record| !record.accepted) {
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
        let decision = existing_record
            .as_ref()
            .map(|record| &record.decision)
            .unwrap_or(&output.decision);
        let mut final_child = child.clone();
        final_child.state = decision.next_state.clone();
        final_child.version = final_child.version.saturating_add(1);
        let commit = store
            .commit_submission_decision_transition(WorkflowSubmissionDecisionTransition {
                workflow_id: &child.id,
                expected_state: &child.state,
                expected_version: child.version,
                create_if_missing: None,
                event_id: Some(&event_id),
                new_event_id: None,
                event_type: "PrFeedbackInspectionRequested",
                source: "workflow_runtime_worker",
                payload: request_payload,
                decision: &output.decision,
                existing_record: existing_record.as_ref(),
                rejection_reason: None,
                final_instance: Some(&final_child),
                command_status: WorkflowCommandStatus::Pending,
                prompt_payload: None,
            })
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "PR feedback child workflow state changed before request could be committed"
                )
            })?;
        if !commit.record.accepted {
            return Ok(ActivityResult::failed(
                activity_name(job),
                "PR feedback child workflow inspection request was rejected.",
                commit
                    .record
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "decision rejected".to_string()),
            )
            .with_error_kind(ActivityErrorKind::Configuration));
        }
        child = final_child;
        let mut command_ids = Vec::new();
        for command_id in commit.command_ids {
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

fn inherit_author_trust_class(
    child: &mut Value,
    parent: &Value,
) -> anyhow::Result<Option<harness_core::config::isolation::IsolationTrustClass>> {
    use harness_core::config::isolation::IsolationTrustClass;

    let Some(value) = parent.get("author_trust_class") else {
        return Ok(None);
    };
    let parent_trust: IsolationTrustClass = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid parent author_trust_class: {error}"))?;
    let child = child
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("child workflow data must be an object"))?;
    let child_trust = child
        .get("author_trust_class")
        .map(|value| {
            serde_json::from_value::<IsolationTrustClass>(value.clone())
                .map_err(|error| anyhow::anyhow!("invalid child author_trust_class: {error}"))
        })
        .transpose()?;
    let effective = if child_trust == Some(IsolationTrustClass::NonCollaborator)
        || parent_trust == IsolationTrustClass::NonCollaborator
    {
        IsolationTrustClass::NonCollaborator
    } else {
        IsolationTrustClass::Trusted
    };
    child.insert(
        "author_trust_class".to_string(),
        serde_json::to_value(effective)?,
    );
    Ok(Some(effective))
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    #[test]
    fn child_inherits_non_collaborator_trust() -> anyhow::Result<()> {
        let mut child = json!({});
        inherit_author_trust_class(
            &mut child,
            &json!({"author_trust_class": "non_collaborator"}),
        )?;
        assert_eq!(child["author_trust_class"], "non_collaborator");
        Ok(())
    }

    #[test]
    fn malformed_parent_trust_fails_closed() {
        let error =
            inherit_author_trust_class(&mut json!({}), &json!({"author_trust_class": "unknown"}))
                .expect_err("invalid trust metadata must fail");
        assert!(error
            .to_string()
            .contains("invalid parent author_trust_class"));
    }

    #[test]
    fn trusted_parent_does_not_downgrade_non_collaborator_child() -> anyhow::Result<()> {
        let mut child = json!({"author_trust_class": "non_collaborator"});
        inherit_author_trust_class(&mut child, &json!({"author_trust_class": "trusted"}))?;
        assert_eq!(child["author_trust_class"], "non_collaborator");
        Ok(())
    }

    #[test]
    fn malformed_child_trust_fails_closed() {
        let error = inherit_author_trust_class(
            &mut json!({"author_trust_class": "unknown"}),
            &json!({"author_trust_class": "trusted"}),
        )
        .expect_err("invalid child trust metadata must fail");
        assert!(error
            .to_string()
            .contains("invalid child author_trust_class"));
    }
}
