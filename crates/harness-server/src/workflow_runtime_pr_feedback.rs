use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_local_review_completed_decision, build_local_review_request_decision,
    build_pr_detected_decision, build_pr_feedback_decision, build_pr_feedback_sweep_decision,
    build_pr_hygiene_repair_decision, DecisionValidator, LocalReviewCompletedInput,
    LocalReviewDecisionInput, LocalReviewOutcome, PrDetectedDecisionInput, PrFeedbackDecisionInput,
    PrFeedbackOutcome, PrFeedbackSweepDecisionInput, PrHygieneRepairDecisionInput,
    ValidationContext, WorkflowCommand, WorkflowCommandStatus, WorkflowCommandType,
    WorkflowDecision, WorkflowDecisionTransition, WorkflowDefinition, WorkflowEvidence,
    WorkflowInstance, WorkflowRejectedDecisionTransition, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID, LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY,
};
use serde_json::json;
use std::path::Path;

const DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS: u64 = 24 * 60 * 60;

mod command_state;
mod persistence;
mod pr_lifecycle_persist;
mod targets;

use command_state::*;
use persistence::*;
use pr_lifecycle_persist::{
    issue_workflow_id, persist_pr_lifecycle_with_retry, pr_lifecycle_workflow_id,
};
#[cfg(test)]
use pr_lifecycle_persist::{
    set_pr_lifecycle_persist_test_failures, PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS,
};
use targets::*;

pub(crate) struct PrDetectedRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: &'a str,
}

pub(crate) struct PrFeedbackRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub outcome: PrFeedbackOutcome,
    pub summary: &'a str,
}

pub(crate) struct LocalReviewPassedRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub summary: &'a str,
}

pub(crate) struct PrMergedRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub summary: &'a str,
}

pub(crate) struct PrFeedbackSweepRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
}

pub(crate) struct PrHygieneRepairRuntimeContext<'a> {
    pub project_root: &'a Path,
    pub repo: Option<&'a str>,
    pub task_id: &'a TaskId,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub title: Option<&'a str>,
    pub merge_state_status: Option<&'a str>,
    pub head_oid: Option<&'a str>,
    pub updated_at: Option<&'a str>,
    pub observed_at: &'a str,
    pub dirty_age_secs: u64,
    pub dirty_age_to_repair_secs: u64,
    pub dirty_age_to_comment_secs: u64,
    pub rebase_needed_label: &'a str,
}

struct PrRuntimeTarget {
    instance: WorkflowInstance,
    new_instance: bool,
    issue_number: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrFeedbackSweepRequestOutcome {
    Requested {
        workflow_id: String,
        task_id: String,
    },
    NotCandidate {
        workflow_id: String,
        state: String,
    },
    ActiveCommandExists {
        workflow_id: String,
        task_id: String,
    },
    Rejected {
        workflow_id: String,
        reason: String,
    },
}

fn runtime_task_id_from_instance(instance: &WorkflowInstance) -> String {
    instance
        .data
        .get("task_id")
        .and_then(|value| value.as_str())
        .filter(|task_id| !task_id.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("runtime:{}", instance.id))
}

fn pr_lifecycle_failure_instance(
    project_root: &Path,
    repo: Option<&str>,
    issue_number: Option<u64>,
    task_id: &TaskId,
    pr_number: u64,
    pr_url: Option<&str>,
) -> WorkflowInstance {
    let project_id = project_root.to_string_lossy().into_owned();
    let mut instance = if let Some(issue_number) = issue_number {
        let workflow_id =
            harness_workflow::issue_lifecycle::workflow_id(&project_id, repo, issue_number);
        issue_instance(
            workflow_id,
            project_id,
            repo.map(ToOwned::to_owned),
            issue_number,
            "failed",
        )
    } else {
        pr_scoped_instance(
            pr_workflow_id(&project_id, repo, pr_number),
            project_id,
            repo.map(ToOwned::to_owned),
            task_id,
            pr_number,
            pr_url,
            "failed",
        )
    };
    if let Some(data) = instance.data.as_object_mut() {
        data.insert("task_id".to_string(), json!(task_id.as_str()));
        data.insert("pr_number".to_string(), json!(pr_number));
        data.insert("pr_url".to_string(), json!(pr_url));
        data.insert(
            "failure_kind".to_string(),
            json!("pr_lifecycle_persistence"),
        );
    }
    instance
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeMergeApprovalOutcome {
    Approved {
        workflow_id: String,
    },
    NotFound,
    NotCandidate {
        workflow_id: String,
        definition_id: String,
    },
    NotReady {
        workflow_id: String,
        state: String,
    },
    Rejected {
        workflow_id: String,
        reason: String,
    },
}

pub(crate) async fn record_pr_detected(
    store: Option<&WorkflowRuntimeStore>,
    ctx: PrDetectedRuntimeContext<'_>,
) {
    let Some(store) = store else {
        tracing::error!(
            issue = ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR detection write skipped because the runtime store is unavailable"
        );
        return;
    };
    let workflow_id = issue_workflow_id(ctx.project_root, ctx.repo, ctx.issue_number);
    let failure_payload = json!({
        "issue_number": ctx.issue_number,
        "repo": ctx.repo,
        "task_id": ctx.task_id.as_str(),
        "pr_number": ctx.pr_number,
        "pr_url": ctx.pr_url,
    });
    let failure_instance = pr_lifecycle_failure_instance(
        ctx.project_root,
        ctx.repo,
        Some(ctx.issue_number),
        ctx.task_id,
        ctx.pr_number,
        Some(ctx.pr_url),
    );
    if let Err(error) = persist_pr_lifecycle_with_retry(
        store,
        &workflow_id,
        "record_pr_detected",
        ctx.task_id,
        ctx.pr_number,
        failure_instance,
        failure_payload,
        || persist_pr_detected(store, &ctx),
    )
    .await
    {
        tracing::error!(
            workflow_id = %workflow_id,
            issue = ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR detection write failed after retries: {error}"
        );
    }
}

pub(crate) async fn record_pr_feedback(
    store: Option<&WorkflowRuntimeStore>,
    ctx: PrFeedbackRuntimeContext<'_>,
) {
    let Some(store) = store else {
        tracing::error!(
            issue = ?ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR feedback write skipped because the runtime store is unavailable"
        );
        return;
    };
    let workflow_id =
        pr_lifecycle_workflow_id(ctx.project_root, ctx.repo, ctx.issue_number, ctx.pr_number);
    let failure_payload = json!({
        "issue_number": ctx.issue_number,
        "repo": ctx.repo,
        "task_id": ctx.task_id.as_str(),
        "pr_number": ctx.pr_number,
        "pr_url": ctx.pr_url,
        "outcome": outcome_label(ctx.outcome),
        "summary": ctx.summary,
    });
    let failure_instance = pr_lifecycle_failure_instance(
        ctx.project_root,
        ctx.repo,
        ctx.issue_number,
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
    );
    if let Err(error) = persist_pr_lifecycle_with_retry(
        store,
        &workflow_id,
        "record_pr_feedback",
        ctx.task_id,
        ctx.pr_number,
        failure_instance,
        failure_payload,
        || persist_pr_feedback(store, &ctx),
    )
    .await
    {
        tracing::error!(
            workflow_id = %workflow_id,
            issue = ?ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR feedback write failed after retries: {error}"
        );
    }
}

pub(crate) async fn record_local_review_passed(
    store: Option<&WorkflowRuntimeStore>,
    ctx: LocalReviewPassedRuntimeContext<'_>,
) {
    let Some(store) = store else {
        return;
    };
    if let Err(error) = persist_local_review_passed(store, &ctx).await {
        tracing::warn!(
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime local review write failed: {error}"
        );
    }
}

pub(crate) async fn record_pr_merged(
    store: Option<&WorkflowRuntimeStore>,
    ctx: PrMergedRuntimeContext<'_>,
) {
    let Some(store) = store else {
        tracing::error!(
            issue = ?ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR merge write skipped because the runtime store is unavailable"
        );
        return;
    };
    let workflow_id =
        pr_lifecycle_workflow_id(ctx.project_root, ctx.repo, ctx.issue_number, ctx.pr_number);
    let failure_payload = json!({
        "issue_number": ctx.issue_number,
        "repo": ctx.repo,
        "task_id": ctx.task_id.as_str(),
        "pr_number": ctx.pr_number,
        "pr_url": ctx.pr_url,
        "summary": ctx.summary,
    });
    let failure_instance = pr_lifecycle_failure_instance(
        ctx.project_root,
        ctx.repo,
        ctx.issue_number,
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
    );
    if let Err(error) = persist_pr_lifecycle_with_retry(
        store,
        &workflow_id,
        "record_pr_merged",
        ctx.task_id,
        ctx.pr_number,
        failure_instance,
        failure_payload,
        || persist_pr_merged(store, &ctx),
    )
    .await
    {
        tracing::error!(
            workflow_id = %workflow_id,
            issue = ?ctx.issue_number,
            pr = ctx.pr_number,
            task_id = %ctx.task_id.0,
            "workflow runtime PR merge write failed after retries: {error}"
        );
    }
}

pub(crate) async fn request_pr_feedback_sweep_for_pr(
    store: &WorkflowRuntimeStore,
    ctx: PrFeedbackSweepRuntimeContext<'_>,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let instance = pr_scoped_instance(
        pr_workflow_id(&project_id, ctx.repo, ctx.pr_number),
        project_id,
        ctx.repo.map(ToOwned::to_owned),
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
        "pr_open",
    );
    upsert_github_issue_pr_definition(store).await?;
    store.upsert_instance(&instance).await?;
    request_local_review(store, &instance.id).await
}

pub(crate) async fn request_pr_hygiene_repair(
    store: &WorkflowRuntimeStore,
    ctx: PrHygieneRepairRuntimeContext<'_>,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let PrRuntimeTarget {
        instance,
        new_instance,
        issue_number,
    } = load_or_pr_runtime_target(
        store,
        ctx.project_root,
        ctx.repo,
        None,
        ctx.pr_number,
        ctx.task_id,
        ctx.pr_url,
        "awaiting_feedback",
    )
    .await?;

    match instance.state.as_str() {
        "awaiting_feedback" | "addressing_feedback" => {}
        "pr_open" => return request_local_review(store, &instance.id).await,
        "local_review_gate" => {
            return Ok(PrFeedbackSweepRequestOutcome::ActiveCommandExists {
                workflow_id: instance.id.clone(),
                task_id: runtime_task_id_from_instance(&instance),
            });
        }
        _ => {
            return Ok(PrFeedbackSweepRequestOutcome::NotCandidate {
                workflow_id: instance.id,
                state: instance.state,
            });
        }
    }

    if has_active_pr_feedback_command_with_activity(store, &instance.id, 0, None).await? {
        return Ok(PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: instance.id.clone(),
            task_id: runtime_task_id_from_instance(&instance),
        });
    }

    persist_pr_hygiene_repair_request(store, instance, new_instance, issue_number, ctx).await
}

pub(crate) async fn request_local_review(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        anyhow::bail!("workflow runtime instance `{workflow_id}` was not found");
    };
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID || instance.state != "pr_open" {
        return Ok(PrFeedbackSweepRequestOutcome::NotCandidate {
            workflow_id: instance.id,
            state: instance.state,
        });
    }
    if has_active_local_review_command(store, &instance.id).await? {
        let task_id = runtime_task_id_from_instance(&instance);
        return Ok(PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: instance.id,
            task_id,
        });
    }
    persist_local_review_request(store, instance).await
}

pub(crate) async fn request_pr_feedback_sweep(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    request_pr_feedback_sweep_with_failed_child_suppression_secs(
        store,
        workflow_id,
        DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS,
    )
    .await
}

pub(crate) async fn request_pr_feedback_sweep_with_failed_child_suppression_secs(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    request_pr_feedback_sweep_with_failed_child_suppression_secs_and_activity(
        store,
        workflow_id,
        failed_child_suppression_secs,
        None,
    )
    .await
}

async fn request_pr_feedback_sweep_with_failed_child_suppression_secs_and_activity(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        anyhow::bail!("workflow runtime instance `{workflow_id}` was not found");
    };
    if instance.definition_id != "github_issue_pr" || instance.state != "awaiting_feedback" {
        return Ok(PrFeedbackSweepRequestOutcome::NotCandidate {
            workflow_id: instance.id,
            state: instance.state,
        });
    }
    if has_active_pr_feedback_command_with_activity(
        store,
        &instance.id,
        failed_child_suppression_secs,
        latest_pr_activity_at,
    )
    .await?
    {
        let task_id = runtime_task_id_from_instance(&instance);
        return Ok(PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            workflow_id: instance.id,
            task_id,
        });
    }
    persist_pr_feedback_sweep_request(store, instance).await
}

async fn persist_local_review_request(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let workflow_id = instance.id.clone();
    let task_id = runtime_task_id_from_instance(&instance);
    let pr_number = required_u64_field(&instance.data, "pr_number")?;
    let pr_url = optional_string_field(&instance.data, "pr_url");
    let issue_number = instance
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let repo = optional_string_field(&instance.data, "repo");
    let accepted_data = instance.data.clone();
    let review_nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let output = build_local_review_request_decision(
        &instance,
        LocalReviewDecisionInput {
            dedupe_key: &format!("local-review:{}:{review_nonce}", instance.id),
            pr_number,
            pr_url: pr_url.as_deref(),
            issue_number,
            repo: repo.as_deref(),
            summary: "Runtime workflow requested local agent review before remote feedback.",
        },
    );
    let event_payload = json!({
        "issue_number": issue_number,
        "repo": repo.as_deref(),
        "pr_number": pr_number,
        "pr_url": pr_url.as_deref(),
    });
    match commit_runtime_decision(
        store,
        instance,
        false,
        output.decision,
        "LocalReviewRequested",
        "workflow_runtime_pr_feedback",
        event_payload,
        accepted_data,
    )
    .await?
    {
        RuntimeDecisionCommitOutcome::Accepted => Ok(PrFeedbackSweepRequestOutcome::Requested {
            workflow_id,
            task_id,
        }),
        RuntimeDecisionCommitOutcome::Rejected { reason } => {
            Ok(PrFeedbackSweepRequestOutcome::Rejected {
                workflow_id,
                reason,
            })
        }
        RuntimeDecisionCommitOutcome::Stale => {
            let state = store
                .get_instance(&workflow_id)
                .await?
                .map(|instance| instance.state)
                .unwrap_or_else(|| "missing".to_string());
            Ok(PrFeedbackSweepRequestOutcome::NotCandidate { workflow_id, state })
        }
    }
}

pub(crate) async fn approve_runtime_merge_by_task_id(
    store: &WorkflowRuntimeStore,
    task_id: &str,
) -> anyhow::Result<RuntimeMergeApprovalOutcome> {
    let Some(instance) = store.get_instance_by_task_id(task_id).await? else {
        return Ok(RuntimeMergeApprovalOutcome::NotFound);
    };
    approve_runtime_merge(store, instance, Some(task_id)).await
}

pub(crate) fn synthesized_pr_feedback_task_id(
    project_id: &str,
    repo: Option<&str>,
    pr_number: u64,
) -> TaskId {
    TaskId::from_str(&format!(
        "github-pr-feedback::{project_id}::repo:{}::pr:{pr_number}:feedback",
        repo.unwrap_or("<none>")
    ))
}

pub(crate) async fn approve_runtime_merge_by_workflow_id(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<RuntimeMergeApprovalOutcome> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(RuntimeMergeApprovalOutcome::NotFound);
    };
    approve_runtime_merge(store, instance, None).await
}

#[cfg(test)]
mod tests;
