//! Private persistence functions, merge approval persistence, and decision
//! commit for the PR feedback runtime.

use super::targets::{
    event_type, load_or_issue_instance, load_or_pr_runtime_target, merge_last_decision,
    merge_runtime_merge_data, optional_bool_field, optional_string_field, outcome_label,
    pr_runtime_data, required_u64_field, runtime_task_id_from_instance,
    upsert_github_issue_pr_definition,
};
use super::{
    request_local_review, LocalReviewPassedRuntimeContext, PrDetectedRuntimeContext,
    PrFeedbackRuntimeContext, PrFeedbackSweepRequestOutcome, PrHygieneRepairRuntimeContext,
    PrMergedRuntimeContext, PrRuntimeTarget, RuntimeMergeApprovalOutcome,
};
use harness_workflow::runtime::{
    build_local_review_completed_decision, build_local_review_request_decision,
    build_pr_detected_decision, build_pr_feedback_decision, build_pr_feedback_sweep_decision,
    build_pr_hygiene_repair_decision, DecisionValidator, LocalReviewCompletedInput,
    LocalReviewDecisionInput, LocalReviewOutcome, PrDetectedDecisionInput, PrFeedbackDecisionInput,
    PrFeedbackSweepDecisionInput, PrHygieneRepairDecisionInput, ValidationContext, WorkflowCommand,
    WorkflowCommandStatus, WorkflowCommandType, WorkflowDecision, WorkflowDecisionTransition,
    WorkflowEvidence, WorkflowInstance, WorkflowRejectedDecisionTransition, WorkflowRuntimeStore,
};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RuntimeDecisionCommitOutcome {
    Accepted,
    Rejected { reason: String },
    Stale,
}

impl RuntimeDecisionCommitOutcome {
    pub(super) fn into_result(self) -> anyhow::Result<()> {
        match self {
            Self::Accepted | Self::Rejected { .. } => Ok(()),
            Self::Stale => anyhow::bail!(
                "workflow state changed before the runtime transition could be committed"
            ),
        }
    }
}

pub(super) async fn persist_local_review_request(
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

pub(super) async fn persist_pr_detected(
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

pub(super) async fn persist_pr_feedback_sweep_request(
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

pub(super) async fn persist_pr_hygiene_repair_request(
    store: &WorkflowRuntimeStore,
    instance: WorkflowInstance,
    new_instance: bool,
    issue_number: Option<u64>,
    ctx: PrHygieneRepairRuntimeContext<'_>,
) -> anyhow::Result<PrFeedbackSweepRequestOutcome> {
    let workflow_id = instance.id.clone();
    let task_id = runtime_task_id_from_instance(&instance);
    let project_id = ctx.project_root.to_string_lossy().into_owned();
    let pr_url = optional_string_field(&instance.data, "pr_url").or_else(|| {
        ctx.pr_url
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });
    let repo = optional_string_field(&instance.data, "repo").or_else(|| {
        ctx.repo
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });
    let summary = format!(
        "Runtime PR hygiene found mergeability repair is needed for PR #{}.",
        ctx.pr_number
    );
    let hygiene_context = json!({
        "source": "pr_hygiene",
        "repo": repo.as_deref(),
        "pr_number": ctx.pr_number,
        "pr_url": pr_url.as_deref(),
        "title": ctx.title,
        "merge_state_status": ctx.merge_state_status,
        "head_oid": ctx.head_oid,
        "updated_at": ctx.updated_at,
        "observed_at": ctx.observed_at,
        "dirty_age_secs": ctx.dirty_age_secs,
        "age_source": "updated_at",
        "dirty_age_to_repair_secs": ctx.dirty_age_to_repair_secs,
        "dirty_age_to_comment_secs": ctx.dirty_age_to_comment_secs,
        "rebase_needed_label": ctx.rebase_needed_label,
        "instructions": [
            "Try a GitHub-side update or rebase for this PR before making unrelated code changes.",
            "If GitHub-side update or rebase fails, apply the configured rebase-needed label.",
            "Escalate when dirty_age_secs is at least dirty_age_to_repair_secs.",
            "Comment asking whether the PR should be closed when dirty_age_secs is at least dirty_age_to_comment_secs and no recent activity explains the stale state."
        ],
    });
    let mut accepted_data = pr_runtime_data(
        ctx.project_root,
        project_id,
        repo.as_deref(),
        issue_number,
        ctx.task_id,
        ctx.pr_number,
        pr_url.as_deref(),
        Some(&summary),
    );
    if let Some(object) = accepted_data.as_object_mut() {
        object.insert("hygiene_context".to_string(), hygiene_context.clone());
    }
    let repair_nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let output = build_pr_hygiene_repair_decision(
        &instance,
        PrHygieneRepairDecisionInput {
            dedupe_key: &format!("pr-hygiene:{}:{repair_nonce}", instance.id),
            pr_number: ctx.pr_number,
            pr_url: pr_url.as_deref(),
            issue_number,
            repo: repo.as_deref(),
            summary: &summary,
            hygiene_context: hygiene_context.clone(),
        },
    );
    let event_payload = json!({
        "issue_number": issue_number,
        "repo": repo.as_deref(),
        "pr_number": ctx.pr_number,
        "pr_url": pr_url.as_deref(),
        "merge_state_status": ctx.merge_state_status,
        "dirty_age_secs": ctx.dirty_age_secs,
    });
    match commit_runtime_decision(
        store,
        instance,
        new_instance,
        output.decision,
        "PrHygieneRepairRequested",
        "workflow_runtime_pr_hygiene",
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

pub(super) async fn persist_pr_feedback(
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

pub(super) async fn persist_local_review_passed(
    store: &WorkflowRuntimeStore,
    ctx: &LocalReviewPassedRuntimeContext<'_>,
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
    if instance.state == "awaiting_feedback" {
        return Ok(());
    }
    if !matches!(instance.state.as_str(), "pr_open" | "local_review_gate") {
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

pub(super) async fn persist_pr_merged(
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

pub(super) async fn approve_runtime_merge(
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
    let pr_number = instance
        .data
        .get("pr_number")
        .and_then(|value| value.as_u64());
    let pr_url = optional_string_field(&instance.data, "pr_url");
    let expected_head_sha = optional_string_field(&instance.data, "pr_head_sha")
        .or_else(|| optional_string_field(&instance.data, "head_sha"));
    let merge_method = optional_string_field(&instance.data, "merge_method")
        .unwrap_or_else(|| "squash".to_string());
    let delete_branch = optional_bool_field(&instance.data, "merge_delete_branch").unwrap_or(true);
    let require_review_threads_resolved =
        optional_bool_field(&instance.data, "merge_require_review_threads_resolved")
            .unwrap_or(true);
    let require_clean_merge_state =
        optional_bool_field(&instance.data, "merge_require_clean_merge_state").unwrap_or(true);
    let last_remote_fact_hash = optional_string_field(&instance.data, "last_remote_fact_hash");
    let merge_dedupe_key = last_remote_fact_hash
        .as_deref()
        .map(|fact_hash| {
            harness_workflow::runtime::remote_fact_command_dedupe_key("merge_pr", fact_hash)
        })
        .unwrap_or_else(|| format!("merge-approved:{}:{approval_nonce}", instance.id));
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "approve_merge",
        "merging",
        "operator approved the ready-to-merge workflow for merge execution",
    )
    .with_command(harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::EnqueueActivity,
        merge_dedupe_key,
        json!({
            "workflow_id": instance.id,
            "task_id": task_id,
            "activity": "merge_pr",
            "approved_by": "dashboard",
            "pr_number": pr_number,
            "pr_url": pr_url,
            "expected_head_sha": expected_head_sha,
            "merge_method": merge_method,
            "delete_branch": delete_branch,
            "require_review_threads_resolved": require_review_threads_resolved,
            "require_clean_merge_state": require_clean_merge_state,
            "dispatch_gate": {
                "reason": "auto_merge_gate_passed",
                "fact_hash": last_remote_fact_hash,
            },
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "operator_approval",
        "dashboard merge approval for ready-to-merge workflow",
    ))
    .high_confidence();
    let accepted_data = merge_runtime_merge_data(
        instance.data.clone(),
        &decision.decision,
        task_id,
        delete_branch,
        require_review_threads_resolved,
        require_clean_merge_state,
    );
    let event_payload = json!({
        "task_id": task_id,
        "issue_number": instance.data.get("issue_number").and_then(|value| value.as_u64()),
        "repo": optional_string_field(&instance.data, "repo"),
        "pr_number": pr_number,
        "pr_url": pr_url,
        "expected_head_sha": expected_head_sha,
        "merge_method": merge_method,
        "delete_branch": delete_branch,
        "require_review_threads_resolved": require_review_threads_resolved,
        "require_clean_merge_state": require_clean_merge_state,
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

pub(super) async fn commit_runtime_decision(
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
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?;
    Ok(match record {
        Some(_) => RuntimeDecisionCommitOutcome::Accepted,
        None => RuntimeDecisionCommitOutcome::Stale,
    })
}
