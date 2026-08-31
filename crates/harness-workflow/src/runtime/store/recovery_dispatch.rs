use super::{has_no_structured_stop_metadata, select_instance_tx};
use crate::runtime::model::{WorkflowCommand, WorkflowCommandType};
use crate::runtime::pr_feedback::{
    LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecoveryDispatchTarget {
    pub(super) state: String,
    pub(super) activity: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct RecoveryDispatchPlan {
    pub(super) target: RecoveryDispatchTarget,
    pub(super) command_source: RecoveryDispatchCommandSource,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum RecoveryDispatchCommandSource {
    Replay(WorkflowCommand),
    LegacyFallback,
    HygieneRepair,
    /// Fully built progress command for a declarative recovery target, built
    /// through the pinned-command path so an agent-contract activity keeps its
    /// contract, prompt, and definition hash; only the dedupe key is assigned
    /// at dispatch time.
    DeclarativeProgress(WorkflowCommand),
}

pub(super) fn recovery_dispatch_target(
    data: &Value,
    activity_name: Option<&str>,
) -> anyhow::Result<Result<RecoveryDispatchTarget, Option<String>>> {
    let activity = activity_name.map(ToOwned::to_owned);
    let Some(activity_name) = activity.as_deref() else {
        if has_no_structured_stop_metadata(data)? {
            return Ok(Ok(RecoveryDispatchTarget {
                state: "implementing".to_string(),
                activity: Some("implement_issue".to_string()),
            }));
        }
        return Ok(Err(activity));
    };
    let target = match activity_name {
        "implement_issue" => RecoveryDispatchTarget {
            state: "implementing".to_string(),
            activity: Some("implement_issue".to_string()),
        },
        "replan_issue" => RecoveryDispatchTarget {
            state: "replanning".to_string(),
            activity: Some("replan_issue".to_string()),
        },
        "merge_pr" => RecoveryDispatchTarget {
            state: "merging".to_string(),
            activity: Some("merge_pr".to_string()),
        },
        LOCAL_REVIEW_ACTIVITY => RecoveryDispatchTarget {
            state: "local_review_gate".to_string(),
            activity: Some(LOCAL_REVIEW_ACTIVITY.to_string()),
        },
        "sweep_pr_feedback" => RecoveryDispatchTarget {
            state: "awaiting_feedback".to_string(),
            activity: Some("sweep_pr_feedback".to_string()),
        },
        PR_FEEDBACK_INSPECT_ACTIVITY => RecoveryDispatchTarget {
            state: "awaiting_feedback".to_string(),
            activity: Some(PR_FEEDBACK_INSPECT_ACTIVITY.to_string()),
        },
        "start_child_workflow" => RecoveryDispatchTarget {
            state: "awaiting_feedback".to_string(),
            activity: Some("start_child_workflow".to_string()),
        },
        "address_pr_feedback" => RecoveryDispatchTarget {
            state: "addressing_feedback".to_string(),
            activity: Some("address_pr_feedback".to_string()),
        },
        _ => return Ok(Err(activity)),
    };
    Ok(Ok(target))
}

pub(super) fn is_hygiene_convergence_stop(data: &Value) -> anyhow::Result<bool> {
    if data.pointer("/last_stop/source").and_then(Value::as_str)
        != Some(crate::runtime::pr_feedback::PR_HYGIENE_CONVERGENCE_STOP_SOURCE)
    {
        return Ok(false);
    }
    let hygiene = data
        .get("hygiene_context")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            anyhow::anyhow!("workflow runtime recovery hygiene_context must be an object")
        })?;
    if data.pointer("/last_stop/state").and_then(Value::as_str) != Some("blocked")
        || data.pointer("/last_stop/activity").and_then(Value::as_str)
            != Some("address_pr_feedback")
        || data.get("pr_number").and_then(Value::as_u64).is_none()
        || data
            .get("feedback_summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_none_or(str::is_empty)
        || hygiene.get("source").and_then(Value::as_str) != Some("pr_hygiene")
    {
        anyhow::bail!("workflow runtime recovery hygiene convergence metadata is incomplete");
    }
    Ok(true)
}

pub(super) async fn select_command_for_runtime_job_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    runtime_job_id: &str,
) -> anyhow::Result<Option<WorkflowCommand>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT command.data::text FROM runtime_jobs AS job JOIN workflow_commands AS command ON command.id = job.command_id WHERE job.id = $1 AND command.workflow_id = $2",
    )
    .bind(runtime_job_id)
    .bind(workflow_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

pub(super) async fn select_parent_command_for_child_job_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    parent_workflow_id: &str,
    child_runtime_job_id: &str,
) -> anyhow::Result<Option<WorkflowCommand>> {
    let child_workflow_id: Option<(String,)> = sqlx::query_as(
        "SELECT command.workflow_id
         FROM runtime_jobs AS job
         JOIN workflow_commands AS command ON command.id = job.command_id
         WHERE job.id = $1",
    )
    .bind(child_runtime_job_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((child_workflow_id,)) = child_workflow_id else {
        return Ok(None);
    };
    let Some(child) = select_instance_tx(tx, &child_workflow_id).await? else {
        return Ok(None);
    };
    if child.parent_workflow_id.as_deref() != Some(parent_workflow_id) {
        return Ok(None);
    }
    let Some(parent_runtime_job_id) = child
        .data
        .get("started_by_runtime_job_id")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    select_command_for_runtime_job_tx(tx, parent_workflow_id, parent_runtime_job_id).await
}

pub(super) fn command_matches_recovery_target(
    command: &WorkflowCommand,
    target: &RecoveryDispatchTarget,
) -> bool {
    match command.command_type {
        WorkflowCommandType::EnqueueActivity => {
            command.activity_name() == target.activity.as_deref()
                && enqueue_payload_matches_target(&command.command)
        }
        WorkflowCommandType::StartChildWorkflow => {
            let payload = &command.command;
            matches!(
                target.activity.as_deref(),
                Some("start_child_workflow" | "sweep_pr_feedback")
            ) && payload.get("definition_id").and_then(Value::as_str)
                == Some(PR_FEEDBACK_DEFINITION_ID)
                && payload.get("child_activity").and_then(Value::as_str)
                    == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
                && payload.get("pr_number").and_then(Value::as_u64).is_some()
                && payload
                    .get("subject_key")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
        }
        _ => false,
    }
}

fn enqueue_payload_matches_target(payload: &Value) -> bool {
    let review_summary = payload
        .get("review_summary")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    let hygiene = payload
        .get("hygiene")
        .or_else(|| payload.get("hygiene_context"))
        .is_some_and(|value| !value.is_null());
    payload.get("source").and_then(Value::as_str) != Some("pr_hygiene")
        || (payload.get("pr_number").and_then(Value::as_u64).is_some() && review_summary && hygiene)
}
