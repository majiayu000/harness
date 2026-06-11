use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    build_local_review_completed_decision, build_local_review_request_decision,
    build_pr_detected_decision, build_pr_feedback_decision, build_pr_feedback_sweep_decision,
    DecisionValidator, LocalReviewCompletedInput, LocalReviewDecisionInput, LocalReviewOutcome,
    PrDetectedDecisionInput, PrFeedbackDecisionInput, PrFeedbackOutcome,
    PrFeedbackSweepDecisionInput, ValidationContext, WorkflowCommand, WorkflowCommandType,
    WorkflowDecision, WorkflowDecisionTransition, WorkflowDefinition, WorkflowEvidence,
    WorkflowInstance, WorkflowRejectedDecisionTransition, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID, LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY,
};
use serde_json::json;
use std::path::Path;

const DEFAULT_PR_FEEDBACK_FAILED_CHILD_SUPPRESSION_SECS: u64 = 24 * 60 * 60;

mod pr_lifecycle_persist;
use pr_lifecycle_persist::{
    issue_workflow_id, persist_pr_lifecycle_with_retry, pr_lifecycle_workflow_id,
};
#[cfg(test)]
use pr_lifecycle_persist::{
    set_pr_lifecycle_persist_test_failures, PR_LIFECYCLE_PERSIST_MAX_ATTEMPTS,
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeDecisionCommitOutcome {
    Accepted,
    Rejected { reason: String },
    Stale,
}

impl RuntimeDecisionCommitOutcome {
    fn into_result(self) -> anyhow::Result<()> {
        match self {
            Self::Accepted | Self::Rejected { .. } => Ok(()),
            Self::Stale => anyhow::bail!(
                "workflow state changed before the runtime transition could be committed"
            ),
        }
    }
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
    if let Err(error) = persist_pr_lifecycle_with_retry(
        store,
        &workflow_id,
        "record_pr_detected",
        ctx.task_id,
        ctx.pr_number,
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
    if let Err(error) = persist_pr_lifecycle_with_retry(
        store,
        &workflow_id,
        "record_pr_feedback",
        ctx.task_id,
        ctx.pr_number,
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
    if let Err(error) = persist_pr_lifecycle_with_retry(
        store,
        &workflow_id,
        "record_pr_merged",
        ctx.task_id,
        ctx.pr_number,
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
        "repo-backlog::{project_id}::repo:{}::pr:{pr_number}:feedback",
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

async fn persist_pr_detected(
    store: &WorkflowRuntimeStore,
    ctx: &PrDetectedRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, ctx.repo, ctx.issue_number);
    upsert_github_issue_pr_definition(store).await?;
    let (instance, new_instance) = load_or_issue_instance(
        store,
        workflow_id,
        project_id.clone(),
        ctx.repo.map(ToOwned::to_owned),
        ctx.issue_number,
        "implementing",
    )
    .await?;
    let accepted_data = crate::workflow_runtime_policy::merge_runtime_retry_policy(
        ctx.project_root,
        json!({
        "project_id": project_id,
        "repo": ctx.repo,
        "issue_number": ctx.issue_number,
        "task_id": ctx.task_id.as_str(),
        "pr_number": ctx.pr_number,
        "pr_url": ctx.pr_url,
        }),
    );
    let output = build_pr_detected_decision(
        &instance,
        PrDetectedDecisionInput {
            task_id: ctx.task_id.as_str(),
            pr_number: ctx.pr_number,
            pr_url: ctx.pr_url,
        },
    );
    commit_runtime_decision(
        store,
        instance,
        new_instance,
        output.decision,
        "PrDetected",
        "workflow_runtime_pr_feedback",
        json!({
            "task_id": ctx.task_id.as_str(),
            "issue_number": ctx.issue_number,
            "repo": ctx.repo,
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
        }),
        accepted_data,
    )
    .await?
    .into_result()
}

async fn persist_pr_feedback_sweep_request(
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
    let sweep_nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let output = build_pr_feedback_sweep_decision(
        &instance,
        PrFeedbackSweepDecisionInput {
            dedupe_key: &format!("pr-feedback-sweep:{}:{sweep_nonce}", instance.id),
            pr_number,
            pr_url: pr_url.as_deref(),
            issue_number,
            repo: repo.as_deref(),
            summary: "Runtime workflow requested a PR feedback sweep.",
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
        "PrFeedbackSweepRequested",
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

async fn persist_pr_feedback(
    store: &WorkflowRuntimeStore,
    ctx: &PrFeedbackRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let PrRuntimeTarget {
        instance,
        new_instance,
        issue_number,
    } = load_or_pr_runtime_target(
        store,
        ctx.project_root,
        ctx.repo,
        ctx.issue_number,
        ctx.pr_number,
        ctx.task_id,
        ctx.pr_url,
        "pr_open",
    )
    .await?;
    if instance.state == "pr_open" {
        store.upsert_instance(&instance).await?;
        request_local_review(store, &instance.id).await?;
        return Ok(());
    }
    if instance.state != "awaiting_feedback" {
        tracing::debug!(
            workflow_id = %instance.id,
            state = %instance.state,
            pr = ctx.pr_number,
            "workflow runtime ignored PR feedback outside awaiting_feedback"
        );
        return Ok(());
    }
    let accepted_data = pr_runtime_data(
        ctx.project_root,
        project_id,
        ctx.repo,
        issue_number,
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
        Some(ctx.summary),
    );
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: ctx.task_id.as_str(),
            pr_number: ctx.pr_number,
            pr_url: ctx.pr_url,
            outcome: ctx.outcome,
            summary: ctx.summary,
        },
    );
    commit_runtime_decision(
        store,
        instance,
        new_instance,
        output.decision,
        event_type(ctx.outcome),
        "workflow_runtime_pr_feedback",
        json!({
            "task_id": ctx.task_id.as_str(),
            "issue_number": issue_number,
            "repo": ctx.repo,
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
            "outcome": outcome_label(ctx.outcome),
            "summary": ctx.summary,
        }),
        accepted_data,
    )
    .await?
    .into_result()
}

async fn persist_local_review_passed(
    store: &WorkflowRuntimeStore,
    ctx: &LocalReviewPassedRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let PrRuntimeTarget {
        mut instance,
        mut new_instance,
        issue_number,
    } = load_or_pr_runtime_target(
        store,
        ctx.project_root,
        ctx.repo,
        ctx.issue_number,
        ctx.pr_number,
        ctx.task_id,
        ctx.pr_url,
        "pr_open",
    )
    .await?;
    if instance.state == "pr_open" {
        let workflow_id = instance.id.clone();
        persist_completed_local_review_gate_record(
            store,
            instance,
            new_instance,
            issue_number,
            ctx,
        )
        .await?;
        let Some(loaded) = store.get_instance(&workflow_id).await? else {
            anyhow::bail!("workflow runtime instance `{workflow_id}` was not found after local review gate record");
        };
        instance = loaded;
        new_instance = false;
    }
    if instance.state == "awaiting_feedback" {
        return Ok(());
    }
    if instance.state != "local_review_gate" {
        tracing::debug!(
            workflow_id = %instance.id,
            state = %instance.state,
            pr = ctx.pr_number,
            "workflow runtime ignored local review pass outside local_review_gate"
        );
        return Ok(());
    }
    let accepted_data = pr_runtime_data(
        ctx.project_root,
        project_id,
        ctx.repo,
        issue_number,
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
        Some(ctx.summary),
    );
    let repair_dedupe_key = format!(
        "local-review:{}:{}:address:passed",
        ctx.task_id.as_str(),
        ctx.pr_number
    );
    let output = build_local_review_completed_decision(
        &instance,
        LocalReviewCompletedInput {
            task_id: ctx.task_id.as_str(),
            pr_number: ctx.pr_number,
            pr_url: ctx.pr_url,
            repair_dedupe_key: &repair_dedupe_key,
            outcome: LocalReviewOutcome::Passed,
            summary: ctx.summary,
        },
    );
    commit_runtime_decision(
        store,
        instance,
        new_instance,
        output.decision,
        "LocalReviewPassed",
        "workflow_runtime_pr_feedback",
        json!({
            "task_id": ctx.task_id.as_str(),
            "issue_number": issue_number,
            "repo": ctx.repo,
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
            "summary": ctx.summary,
        }),
        accepted_data,
    )
    .await?
    .into_result()
}

async fn persist_completed_local_review_gate_record(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    new_instance: bool,
    issue_number: Option<u64>,
    ctx: &LocalReviewPassedRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let accepted_data = pr_runtime_data(
        ctx.project_root,
        project_id,
        ctx.repo,
        issue_number,
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
        Some(ctx.summary),
    );
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "record_completed_local_review_gate",
        "local_review_gate",
        "Local review already passed; record the local review gate before remote feedback.",
    )
    .with_evidence(WorkflowEvidence::new("local_review", ctx.summary))
    .high_confidence();
    commit_runtime_decision(
        store,
        instance,
        new_instance,
        decision,
        "LocalReviewGateRecorded",
        "workflow_runtime_pr_feedback",
        json!({
            "task_id": ctx.task_id.as_str(),
            "issue_number": issue_number,
            "repo": ctx.repo,
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
            "summary": ctx.summary,
        }),
        accepted_data,
    )
    .await?
    .into_result()
}

async fn persist_pr_merged(
    store: &WorkflowRuntimeStore,
    ctx: &PrMergedRuntimeContext<'_>,
) -> anyhow::Result<()> {
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let PrRuntimeTarget {
        instance,
        new_instance,
        issue_number,
    } = load_or_pr_runtime_target(
        store,
        ctx.project_root,
        ctx.repo,
        ctx.issue_number,
        ctx.pr_number,
        ctx.task_id,
        ctx.pr_url,
        "pr_open",
    )
    .await?;
    let accepted_data = pr_runtime_data(
        ctx.project_root,
        project_id,
        ctx.repo,
        issue_number,
        ctx.task_id,
        ctx.pr_number,
        ctx.pr_url,
        None,
    );
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "record_pr_merged",
        "done",
        ctx.summary,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        format!("pr-merged:{}:{}", ctx.task_id.as_str(), ctx.pr_number),
        json!({
            "task_id": ctx.task_id.as_str(),
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "pr_merged",
        ctx.pr_url.unwrap_or("merged externally"),
    ))
    .high_confidence();
    commit_runtime_decision(
        store,
        instance,
        new_instance,
        decision,
        "PrMerged",
        "workflow_runtime_pr_feedback",
        json!({
            "task_id": ctx.task_id.as_str(),
            "issue_number": issue_number,
            "repo": ctx.repo,
            "pr_number": ctx.pr_number,
            "pr_url": ctx.pr_url,
        }),
        accepted_data,
    )
    .await?
    .into_result()
}

async fn approve_runtime_merge(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    task_id: Option<&str>,
) -> anyhow::Result<RuntimeMergeApprovalOutcome> {
    if instance.definition_id != harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID {
        return Ok(RuntimeMergeApprovalOutcome::NotCandidate {
            workflow_id: instance.id,
            definition_id: instance.definition_id,
        });
    }
    if instance.state != "ready_to_merge" {
        return Ok(RuntimeMergeApprovalOutcome::NotReady {
            workflow_id: instance.id,
            state: instance.state,
        });
    }

    let workflow_id = instance.id.clone();
    let approval_nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "approve_merge",
        "done",
        "operator approved the ready-to-merge workflow",
    )
    .with_command(harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::MarkDone,
        format!("merge-approved:{}:{approval_nonce}", instance.id),
        json!({
            "workflow_id": instance.id,
            "task_id": task_id,
            "approved_by": "dashboard",
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "operator_approval",
        "dashboard merge approval for ready-to-merge workflow",
    ))
    .high_confidence();
    let accepted_data =
        merge_runtime_merge_data(instance.data.clone(), &decision.decision, task_id);
    let event_payload = json!({
        "task_id": task_id,
        "issue_number": instance.data.get("issue_number").and_then(|value| value.as_u64()),
        "repo": optional_string_field(&instance.data, "repo"),
        "pr_number": instance.data.get("pr_number").and_then(|value| value.as_u64()),
        "pr_url": optional_string_field(&instance.data, "pr_url"),
    });
    match commit_runtime_decision(
        store,
        instance,
        false,
        decision,
        "MergeApproved",
        "workflow_runtime_dashboard",
        event_payload,
        accepted_data,
    )
    .await?
    {
        RuntimeDecisionCommitOutcome::Accepted => {
            Ok(RuntimeMergeApprovalOutcome::Approved { workflow_id })
        }
        RuntimeDecisionCommitOutcome::Rejected { reason } => {
            Ok(RuntimeMergeApprovalOutcome::Rejected {
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
            Ok(RuntimeMergeApprovalOutcome::NotReady { workflow_id, state })
        }
    }
}

async fn commit_runtime_decision(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    new_instance: bool,
    decision: WorkflowDecision,
    event_type: &'static str,
    source: &'static str,
    event_payload: serde_json::Value,
    accepted_data: serde_json::Value,
) -> anyhow::Result<RuntimeDecisionCommitOutcome> {
    let expected_state = instance.state.clone();
    let validator = DecisionValidator::github_issue_pr();
    let validation = validator.validate(
        &instance,
        &decision,
        &ValidationContext::new("workflow-policy", chrono::Utc::now()),
    );
    if let Err(error) = validation {
        let reason = error.to_string();
        let record = store
            .record_rejected_decision_transition(WorkflowRejectedDecisionTransition {
                expected_state: &expected_state,
                create_if_missing: new_instance.then_some(&instance),
                event_type,
                source,
                payload: event_payload,
                decision: &decision,
                reason: &reason,
            })
            .await?;
        return Ok(match record {
            Some(_) => RuntimeDecisionCommitOutcome::Rejected { reason },
            None => RuntimeDecisionCommitOutcome::Stale,
        });
    }

    let mut final_instance = instance.clone();
    final_instance.state = decision.next_state.clone();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data = merge_last_decision(accepted_data, &decision.decision);
    let create_if_missing = new_instance.then_some(&instance);
    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: &expected_state,
            create_if_missing,
            event_type,
            source,
            payload: event_payload,
            decision: &decision,
            final_instance: &final_instance,
            command_status: "pending",
        })
        .await?;
    Ok(match record {
        Some(_) => RuntimeDecisionCommitOutcome::Accepted,
        None => RuntimeDecisionCommitOutcome::Stale,
    })
}

fn is_active_pr_feedback_command_status(status: &str) -> bool {
    matches!(status, "pending" | "dispatching" | "dispatched")
}

async fn has_active_local_review_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            is_active_pr_feedback_command_status(record.status.as_str())
                && record.command.activity_name() == Some(LOCAL_REVIEW_ACTIVITY)
        }))
}

#[cfg(test)]
async fn has_active_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
) -> anyhow::Result<bool> {
    has_active_pr_feedback_command_with_activity(
        store,
        workflow_id,
        failed_child_suppression_secs,
        None,
    )
    .await
}

async fn has_active_pr_feedback_command_with_activity(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    failed_child_suppression_secs: u64,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<bool> {
    let parent_has_active_command =
        store
            .commands_for(workflow_id)
            .await?
            .into_iter()
            .any(|record| {
                is_active_pr_feedback_command_status(record.status.as_str())
                    && matches!(
                        record.command.activity_name(),
                        Some("sweep_pr_feedback" | "address_pr_feedback")
                    )
                    || is_active_pr_feedback_command_status(record.status.as_str())
                        && record.command.command_type
                            == harness_workflow::runtime::WorkflowCommandType::StartChildWorkflow
                        && record
                            .command
                            .command
                            .get("definition_id")
                            .and_then(|value| value.as_str())
                            == Some(PR_FEEDBACK_DEFINITION_ID)
            });
    if parent_has_active_command {
        return Ok(true);
    }

    // Scope the child lookup to this parent's children via the indexed-by-value
    // `parent_workflow_id` column, rather than loading every PR-feedback instance
    // across all projects and filtering in memory (which scales with the whole
    // table and inflates memory use).
    for instance in store
        .list_instances_by_parent(workflow_id, None)
        .await?
        .into_iter()
        .filter(|instance| instance.definition_id == PR_FEEDBACK_DEFINITION_ID)
    {
        if matches!(instance.state.as_str(), "pending" | "inspecting")
            && has_active_child_pr_feedback_command(store, &instance.id).await?
        {
            return Ok(true);
        }
        if matches!(instance.state.as_str(), "failed" | "blocked")
            && has_recent_failed_child_pr_feedback_command(
                store,
                &instance.id,
                failed_child_suppression_secs,
                latest_pr_activity_at,
            )
            .await?
        {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn has_recent_failed_child_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    suppression_secs: u64,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<bool> {
    let Some(cutoff) = failed_child_suppression_cutoff(suppression_secs) else {
        return Ok(false);
    };
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            matches!(record.status.as_str(), "failed" | "blocked")
                && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
                && record.updated_at >= cutoff
                && failed_child_suppression_still_applies(record.updated_at, latest_pr_activity_at)
        }))
}

fn failed_child_suppression_still_applies(
    failed_command_updated_at: chrono::DateTime<chrono::Utc>,
    latest_pr_activity_at: Option<chrono::DateTime<chrono::Utc>>,
) -> bool {
    latest_pr_activity_at
        .map(|activity_at| activity_at < failed_command_updated_at)
        .unwrap_or(true)
}

fn failed_child_suppression_cutoff(suppression_secs: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    if suppression_secs == 0 {
        return None;
    }
    let now = chrono::Utc::now();
    Some(
        i64::try_from(suppression_secs)
            .ok()
            .and_then(chrono::Duration::try_seconds)
            .and_then(|duration| now.checked_sub_signed(duration))
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC),
    )
}

async fn has_active_child_pr_feedback_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<bool> {
    Ok(store
        .commands_for(workflow_id)
        .await?
        .into_iter()
        .any(|record| {
            is_active_pr_feedback_command_status(record.status.as_str())
                && record.command.activity_name() == Some(PR_FEEDBACK_INSPECT_ACTIVITY)
        }))
}

async fn upsert_github_issue_pr_definition(store: &WorkflowRuntimeStore) -> anyhow::Result<()> {
    store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
        ))
        .await
}

async fn load_or_issue_instance(
    store: &WorkflowRuntimeStore,
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> anyhow::Result<(WorkflowInstance, bool)> {
    Ok(match store.get_instance(&workflow_id).await? {
        Some(instance) => (instance, false),
        None => (
            issue_instance(workflow_id, project_id, repo, issue_number, state),
            true,
        ),
    })
}

async fn load_or_pr_runtime_target(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    repo: Option<&str>,
    issue_number: Option<u64>,
    pr_number: u64,
    task_id: &TaskId,
    pr_url: Option<&str>,
    state: &str,
) -> anyhow::Result<PrRuntimeTarget> {
    upsert_github_issue_pr_definition(store).await?;
    let project_id = project_root.to_string_lossy().into_owned();
    if let Some(issue_number) = issue_number {
        let workflow_id =
            harness_workflow::issue_lifecycle::workflow_id(&project_id, repo, issue_number);
        let (instance, new_instance) = load_or_issue_instance(
            store,
            workflow_id,
            project_id,
            repo.map(ToOwned::to_owned),
            issue_number,
            state,
        )
        .await?;
        return Ok(PrRuntimeTarget {
            instance,
            new_instance,
            issue_number: Some(issue_number),
        });
    }

    if let Some(instance) = store
        .get_instance_by_pr(GITHUB_ISSUE_PR_DEFINITION_ID, &project_id, repo, pr_number)
        .await?
    {
        let issue_number = instance
            .data
            .get("issue_number")
            .and_then(serde_json::Value::as_u64);
        return Ok(PrRuntimeTarget {
            instance,
            new_instance: false,
            issue_number,
        });
    }

    Ok(PrRuntimeTarget {
        instance: pr_scoped_instance(
            pr_workflow_id(&project_id, repo, pr_number),
            project_id,
            repo.map(ToOwned::to_owned),
            task_id,
            pr_number,
            pr_url,
            state,
        ),
        new_instance: true,
        issue_number: None,
    })
}

fn issue_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    issue_number: u64,
    state: &str,
) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        state,
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id)
    .with_data(crate::workflow_runtime_policy::merge_runtime_retry_policy(
        Path::new(&project_id),
        json!({
            "project_id": project_id,
            "repo": repo,
            "issue_number": issue_number,
        }),
    ))
}

fn pr_scoped_instance(
    workflow_id: String,
    project_id: String,
    repo: Option<String>,
    task_id: &TaskId,
    pr_number: u64,
    pr_url: Option<&str>,
    state: &str,
) -> WorkflowInstance {
    let data = pr_runtime_data(
        Path::new(&project_id),
        project_id.clone(),
        repo.as_deref(),
        None,
        task_id,
        pr_number,
        pr_url,
        None,
    );
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        state,
        WorkflowSubject::new("pr", format!("pr:{pr_number}")),
    )
    .with_id(workflow_id)
    .with_data(data)
}

fn pr_workflow_id(project_id: &str, repo: Option<&str>, pr_number: u64) -> String {
    format!(
        "{project_id}::repo:{}::pr:{pr_number}:feedback",
        repo.unwrap_or("<none>")
    )
}

fn pr_runtime_data(
    project_root: &Path,
    project_id: String,
    repo: Option<&str>,
    issue_number: Option<u64>,
    task_id: &TaskId,
    pr_number: u64,
    pr_url: Option<&str>,
    feedback_summary: Option<&str>,
) -> serde_json::Value {
    let mut data = json!({
        "project_id": project_id,
        "repo": repo,
        "task_id": task_id.as_str(),
        "pr_number": pr_number,
        "pr_url": pr_url,
    });
    if let Some(issue_number) = issue_number {
        data["issue_number"] = json!(issue_number);
    }
    if let Some(feedback_summary) = feedback_summary {
        data["feedback_summary"] = json!(feedback_summary);
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(project_root, data)
}

fn merge_last_decision(mut data: serde_json::Value, decision: &str) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
    }
    data
}

fn merge_runtime_merge_data(
    mut data: serde_json::Value,
    decision: &str,
    task_id: Option<&str>,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
        object.insert("merge_approved_at".to_string(), json!(chrono::Utc::now()));
        if let Some(task_id) = task_id {
            object.insert("merge_approved_task_id".to_string(), json!(task_id));
        }
    }
    data
}

fn optional_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn required_u64_field(data: &serde_json::Value, field: &str) -> anyhow::Result<u64> {
    data.get(field)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| anyhow::anyhow!("runtime issue workflow is missing {field}"))
}

fn event_type(outcome: PrFeedbackOutcome) -> &'static str {
    match outcome {
        PrFeedbackOutcome::BlockingFeedback => "FeedbackFound",
        PrFeedbackOutcome::NoActionableFeedback => "NoFeedbackFound",
        PrFeedbackOutcome::ReadyToMerge => "PrReadyToMerge",
    }
}

fn outcome_label(outcome: PrFeedbackOutcome) -> &'static str {
    match outcome {
        PrFeedbackOutcome::BlockingFeedback => "blocking_feedback",
        PrFeedbackOutcome::NoActionableFeedback => "no_actionable_feedback",
        PrFeedbackOutcome::ReadyToMerge => "ready_to_merge",
    }
}

#[cfg(test)]
mod tests;
