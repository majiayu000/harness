use super::candidate_selection::{
    CandidateOutcome, CandidatePromotionRecord, CandidateRankingRecord, CandidateSelectionRecord,
    CANDIDATE_SELECTION_RECORD_TYPE,
};
use super::model::{
    ActivityArtifact, ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;

pub const CANDIDATE_BRANCH_ARTIFACT: &str = "candidate_branch";
pub const CANDIDATE_PROMOTION_ACTIVITY: &str = "promote_candidate_pr";
pub const CANDIDATE_CLEANUP_ACTIVITY: &str = "cleanup_candidate_workspaces";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePromotionFailure {
    pub candidate_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePromotionTarget {
    pub candidate_id: String,
    pub rank: usize,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_index: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatePromotionPlan {
    pub candidate_group_id: String,
    pub selected: CandidatePromotionTarget,
    pub cleanup_targets: Vec<CandidatePromotionTarget>,
    pub failed_promotions: Vec<CandidatePromotionFailure>,
    pub promotion_chain: Vec<CandidatePromotionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidatePromotionPlanError {
    EmptyCandidateGroup,
    MissingBranch { candidate_id: String },
    NoPromotableCandidate,
}

impl fmt::Display for CandidatePromotionPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCandidateGroup => write!(formatter, "candidate_group_id must not be empty"),
            Self::MissingBranch { candidate_id } => {
                write!(formatter, "candidate {candidate_id} is missing a branch")
            }
            Self::NoPromotableCandidate => write!(formatter, "no promotable candidate remains"),
        }
    }
}

impl std::error::Error for CandidatePromotionPlanError {}

pub fn candidate_selection_record_from_activity_result(
    result: &ActivityResult,
) -> Option<anyhow::Result<CandidateSelectionRecord>> {
    result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == CANDIDATE_SELECTION_RECORD_TYPE)
        .map(|artifact| {
            serde_json::from_value::<CandidateSelectionRecord>(artifact.artifact.clone())
                .map_err(Into::into)
        })
}

pub fn candidate_promotion_plan(
    selection: &CandidateSelectionRecord,
    failed_promotions: Vec<CandidatePromotionFailure>,
) -> Result<CandidatePromotionPlan, CandidatePromotionPlanError> {
    if selection.candidate_group_id.trim().is_empty() {
        return Err(CandidatePromotionPlanError::EmptyCandidateGroup);
    }
    let failed_ids = failed_candidate_ids(&failed_promotions);
    let selected = selection
        .ranking
        .iter()
        .filter(|ranking| ranking.outcome == CandidateOutcome::Succeeded)
        .filter(|ranking| !failed_ids.contains(ranking.candidate_id.as_str()))
        .min_by_key(|ranking| ranking.rank)
        .ok_or(CandidatePromotionPlanError::NoPromotableCandidate)
        .and_then(target_from_ranking)?;

    let cleanup_targets = selection
        .ranking
        .iter()
        .filter(|ranking| ranking.candidate_id != selected.candidate_id)
        .filter_map(|ranking| target_from_ranking(ranking).ok())
        .collect::<Vec<_>>();
    let promotion_chain = promotion_chain(selection, &failed_promotions, &selected);

    Ok(CandidatePromotionPlan {
        candidate_group_id: selection.candidate_group_id.clone(),
        selected,
        cleanup_targets,
        failed_promotions,
        promotion_chain,
    })
}

pub fn build_candidate_promotion_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    selection: CandidateSelectionRecord,
    failed_promotions: Vec<CandidatePromotionFailure>,
) -> Result<WorkflowDecision, CandidatePromotionPlanError> {
    let plan = candidate_promotion_plan(&selection, failed_promotions)?;
    let decision_name = if plan.failed_promotions.is_empty() {
        "promote_candidate_pr"
    } else {
        "promote_runner_up_candidate_pr"
    };
    let reason = format!(
        "candidate selection chose {} for PR promotion",
        plan.selected.candidate_id
    );
    Ok(WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        &instance.state,
        &reason,
    )
    .with_command(promote_candidate_command(&plan, &selection, &event.id))
    .with_evidence(WorkflowEvidence::new(
        CANDIDATE_SELECTION_RECORD_TYPE,
        format!(
            "candidate_group_id={} selected_candidate_id={}",
            plan.candidate_group_id, plan.selected.candidate_id
        ),
    ))
    .with_evidence(WorkflowEvidence::new(
        "runtime_completion",
        format!("event_id={} activity={}", event.id, result.activity),
    ))
    .high_confidence())
}

pub fn deferred_candidate_result_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> Option<anyhow::Result<WorkflowDecision>> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        "implement_issue",
    ) {
        return None;
    }
    if command
        .command
        .get("submission_mode")
        .and_then(Value::as_str)
        != Some("deferred")
    {
        return None;
    }
    let Some(candidate) = command.command.get("candidate") else {
        return Some(Err(anyhow::anyhow!(
            "deferred implement_issue command is missing candidate metadata"
        )));
    };
    let candidate_id = candidate
        .get("candidate_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("candidate metadata is missing candidate_id"));
    let candidate_id = match candidate_id {
        Ok(candidate_id) => candidate_id,
        Err(error) => return Some(Err(error)),
    };
    if !has_candidate_completion_artifact(result) {
        return Some(Err(anyhow::anyhow!(
            "deferred implement_issue succeeded without a candidate_branch artifact"
        )));
    }
    Some(Ok(WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "record_deferred_candidate_result",
        &instance.state,
        "deferred candidate produced branch evidence without binding a PR",
    )
    .with_evidence(WorkflowEvidence::new(
        "candidate",
        format!("candidate_id={candidate_id}"),
    ))
    .with_evidence(WorkflowEvidence::new(
        "runtime_completion",
        format!("event_id={} activity={}", event.id, result.activity),
    ))
    .high_confidence()))
}

pub fn candidate_promotion_success_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> Option<anyhow::Result<WorkflowDecision>> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        CANDIDATE_PROMOTION_ACTIVITY,
    ) {
        return None;
    }
    Some(candidate_promotion_success_decision_inner(
        instance, event, result, command,
    ))
}

fn candidate_promotion_success_decision_inner(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> anyhow::Result<WorkflowDecision> {
    let (pr_number, pr_url) = pull_request_artifact(result)
        .ok_or_else(|| anyhow::anyhow!("promote_candidate_pr succeeded without pull_request"))?;
    let context = promotion_command_context(command)?;
    let plan = candidate_promotion_plan(&context.selection, context.failed_promotions)?;
    if plan.selected.candidate_id != context.candidate_id {
        anyhow::bail!(
            "promote_candidate_pr completed for {}, but selection now points at {}",
            context.candidate_id,
            plan.selected.candidate_id
        );
    }

    let reason = format!(
        "promoted candidate {} opened the selected pull request",
        plan.selected.candidate_id
    );
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "bind_promoted_candidate_pr",
        "pr_open",
        &reason,
    )
    .with_command(WorkflowCommand::bind_pr(
        pr_number,
        pr_url.clone(),
        format!("candidate-promotion:{}:bind-pr:{pr_number}", event.id),
    ))
    .with_evidence(WorkflowEvidence::new("pull_request", pr_url))
    .with_evidence(WorkflowEvidence::new(
        "candidate",
        format!("candidate_id={}", plan.selected.candidate_id),
    ))
    .with_evidence(WorkflowEvidence::new(
        "runtime_completion",
        format!("event_id={} activity={}", event.id, result.activity),
    ));

    if !plan.cleanup_targets.is_empty() {
        decision = decision.with_command(cleanup_candidate_command(&plan, &event.id));
    }
    Ok(decision.high_confidence())
}

pub fn candidate_promotion_failure_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> Option<anyhow::Result<WorkflowDecision>> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        super::reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
        "implementing",
        CANDIDATE_PROMOTION_ACTIVITY,
    ) {
        return None;
    }
    Some(candidate_promotion_failure_decision_inner(
        instance, event, result, command,
    ))
}

fn candidate_promotion_failure_decision_inner(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    command: &WorkflowCommand,
) -> anyhow::Result<WorkflowDecision> {
    let context = promotion_command_context(command)?;
    let mut failures = context.failed_promotions;
    failures.push(CandidatePromotionFailure {
        candidate_id: context.candidate_id,
        reason: result
            .error
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(result.summary.as_str())
            .to_string(),
    });

    match build_candidate_promotion_decision(
        instance,
        event,
        result,
        context.selection.clone(),
        failures.clone(),
    ) {
        Ok(decision) => Ok(decision),
        Err(CandidatePromotionPlanError::NoPromotableCandidate) => {
            let reason = "candidate PR promotion failed for all succeeded candidates";
            let mut payload = failed_candidate_promotion_payload(reason, event, result);
            if let Some(object) = payload.as_object_mut() {
                object.insert("failed_promotions".to_string(), json!(failures));
            }
            Ok(WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "fail_candidate_promotion",
                "failed",
                reason,
            )
            .with_command(WorkflowCommand::new(
                WorkflowCommandType::MarkFailed,
                format!("candidate-promotion:{}:all-failed", event.id),
                payload,
            ))
            .with_evidence(WorkflowEvidence::new(
                CANDIDATE_SELECTION_RECORD_TYPE,
                context.selection.candidate_group_id,
            ))
            .high_confidence())
        }
        Err(error) => Err(error.into()),
    }
}

fn failed_candidate_promotion_payload(
    reason: &str,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Value {
    let mut payload = json!({
        "reason": reason,
        "failure_reason": reason,
        "activity": result.activity,
        "retry_hint": "Fix the candidate promotion failures, then call the workflow runtime retry API.",
        "last_stop": {
            "state": "failed",
            "activity": result.activity,
            "runtime_job_id": event.event.get("runtime_job_id").and_then(Value::as_str),
            "event_id": event.id,
            "recorded_at": event.created_at,
        },
    });
    if let Some(error_kind) = result.error_kind {
        if let Some(object) = payload.as_object_mut() {
            object.insert("error_kind".to_string(), json!(error_kind));
        }
        if let Some(last_stop) = payload.get_mut("last_stop").and_then(Value::as_object_mut) {
            last_stop.insert("error_kind".to_string(), json!(error_kind));
        }
    }
    payload
}

fn promote_candidate_command(
    plan: &CandidatePromotionPlan,
    selection: &CandidateSelectionRecord,
    dedupe_seed: &str,
) -> WorkflowCommand {
    WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!(
            "candidate-promotion:{}:{}:promote:{}:{}",
            plan.candidate_group_id,
            dedupe_seed,
            plan.selected.candidate_id,
            plan.failed_promotions.len()
        ),
        json!({
            "activity": CANDIDATE_PROMOTION_ACTIVITY,
            "candidate_group_id": plan.candidate_group_id,
            "candidate": plan.selected,
            "selection": selection,
            "promotion_chain": plan.promotion_chain,
            "failed_promotions": plan.failed_promotions,
            "submission_mode": "immediate",
        }),
    )
}

fn cleanup_candidate_command(plan: &CandidatePromotionPlan, dedupe_seed: &str) -> WorkflowCommand {
    WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!(
            "candidate-promotion:{}:{}:cleanup:{}",
            plan.candidate_group_id, dedupe_seed, plan.selected.candidate_id
        ),
        json!({
            "activity": CANDIDATE_CLEANUP_ACTIVITY,
            "candidate_group_id": plan.candidate_group_id,
            "selected_candidate_id": plan.selected.candidate_id,
            "selected_branch": plan.selected.branch,
            "candidates": plan.cleanup_targets,
        }),
    )
}

fn promotion_chain(
    selection: &CandidateSelectionRecord,
    failed_promotions: &[CandidatePromotionFailure],
    selected: &CandidatePromotionTarget,
) -> Vec<CandidatePromotionRecord> {
    let mut chain = selection.promotion_chain.clone();
    let Some(last_failed) = failed_promotions.last() else {
        return chain;
    };
    if last_failed.candidate_id == selected.candidate_id {
        return chain;
    }
    chain.push(CandidatePromotionRecord {
        from_candidate_id: last_failed.candidate_id.clone(),
        to_candidate_id: selected.candidate_id.clone(),
        reason: last_failed.reason.clone(),
    });
    chain
}

fn target_from_ranking(
    ranking: &CandidateRankingRecord,
) -> Result<CandidatePromotionTarget, CandidatePromotionPlanError> {
    let branch = ranking
        .branch
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CandidatePromotionPlanError::MissingBranch {
            candidate_id: ranking.candidate_id.clone(),
        })?;
    Ok(CandidatePromotionTarget {
        candidate_id: ranking.candidate_id.clone(),
        rank: ranking.rank,
        branch: branch.to_string(),
        candidate_index: candidate_index_from_id(&ranking.candidate_id),
    })
}

fn failed_candidate_ids(failures: &[CandidatePromotionFailure]) -> BTreeSet<&str> {
    failures
        .iter()
        .map(|failure| failure.candidate_id.as_str())
        .collect()
}

fn candidate_index_from_id(candidate_id: &str) -> Option<u32> {
    candidate_id
        .rsplit_once(":c")
        .and_then(|(_, suffix)| suffix.parse::<u32>().ok())
        .filter(|index| *index > 0)
}

fn has_candidate_completion_artifact(result: &ActivityResult) -> bool {
    result.artifacts.iter().any(|artifact| {
        matches!(
            artifact.artifact_type.as_str(),
            CANDIDATE_BRANCH_ARTIFACT | "pull_request"
        )
    })
}

fn promotion_command_context(command: &WorkflowCommand) -> anyhow::Result<PromotionCommandContext> {
    let selection = command
        .command
        .get("selection")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("candidate promotion command is missing selection"))?;
    let selection = serde_json::from_value::<CandidateSelectionRecord>(selection)?;
    let candidate_id = command
        .command
        .pointer("/candidate/candidate_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("candidate promotion command is missing candidate_id"))?
        .to_string();
    let failed_promotions = command
        .command
        .get("failed_promotions")
        .cloned()
        .map(serde_json::from_value::<Vec<CandidatePromotionFailure>>)
        .transpose()?
        .unwrap_or_default();
    Ok(PromotionCommandContext {
        selection,
        candidate_id,
        failed_promotions,
    })
}

struct PromotionCommandContext {
    selection: CandidateSelectionRecord,
    candidate_id: String,
    failed_promotions: Vec<CandidatePromotionFailure>,
}

fn pull_request_artifact(result: &ActivityResult) -> Option<(u64, String)> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "pull_request")
        .find_map(valid_pull_request_artifact)
}

fn valid_pull_request_artifact(artifact: &ActivityArtifact) -> Option<(u64, String)> {
    let pr_number = artifact.artifact.get("pr_number")?.as_u64()?;
    let pr_url = artifact.artifact.get("pr_url")?.as_str()?;
    if pr_url.trim().is_empty() {
        return None;
    }
    Some((pr_number, pr_url.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ActivityStatus, CandidateCheckConclusion, CandidateDiffScope, WorkflowSubject,
        GITHUB_ISSUE_PR_DEFINITION_ID, RUNTIME_JOB_COMPLETED_EVENT,
    };

    fn selection_record() -> CandidateSelectionRecord {
        let input = crate::runtime::CandidateSelectionInput {
            candidate_group_id: "wf-1:candidate-group:issue-1449".to_string(),
            candidates: vec![
                crate::runtime::CandidateEvidence::succeeded("wf-1:candidate-group:issue-1449:c1")
                    .with_branch("harness/issue-1449-c1")
                    .with_validation(CandidateCheckConclusion::Passed)
                    .with_ci(CandidateCheckConclusion::Passed)
                    .with_quality_score(90)
                    .with_diff(CandidateDiffScope::Sane, 2, 20),
                crate::runtime::CandidateEvidence::succeeded("wf-1:candidate-group:issue-1449:c2")
                    .with_branch("harness/issue-1449-c2")
                    .with_validation(CandidateCheckConclusion::Passed)
                    .with_ci(CandidateCheckConclusion::Unknown)
                    .with_quality_score(80)
                    .with_diff(CandidateDiffScope::Sane, 3, 30),
            ],
            promotion_chain: Vec::new(),
        };
        crate::runtime::select_candidate(input)
    }

    fn issue_instance() -> WorkflowInstance {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:1449"),
        )
        .with_id("workflow-1449")
    }

    fn event(command: WorkflowCommand) -> WorkflowEvent {
        WorkflowEvent::new("workflow-1449", 1, RUNTIME_JOB_COMPLETED_EVENT, "test").with_payload(
            json!({
                "runtime_job_id": "job-1",
                "command_id": "command-1",
                "command": command,
            }),
        )
    }

    #[test]
    fn candidate_promotion_creates_exactly_one_pr_promotion_command() -> anyhow::Result<()> {
        let selection = selection_record();
        let result = ActivityResult::succeeded("select_candidate", "candidate selected")
            .with_artifact(ActivityArtifact::new(
                CANDIDATE_SELECTION_RECORD_TYPE,
                serde_json::to_value(&selection)?,
            ));
        let event = event(WorkflowCommand::enqueue_activity(
            "select_candidate",
            "select-1",
        ));

        let decision = build_candidate_promotion_decision(
            &issue_instance(),
            &event,
            &result,
            selection,
            Vec::new(),
        )?;

        let promote_commands = decision
            .commands
            .iter()
            .filter(|command| command.command["activity"] == CANDIDATE_PROMOTION_ACTIVITY)
            .count();
        assert_eq!(promote_commands, 1);
        assert_eq!(
            decision.commands[0].command["candidate"]["candidate_index"],
            1
        );
        assert_eq!(
            decision.commands[0].command["candidate"]["branch"],
            "harness/issue-1449-c1"
        );
        Ok(())
    }

    #[test]
    fn candidate_promotion_failure_promotes_runner_up() -> anyhow::Result<()> {
        let selection = selection_record();
        let first = candidate_promotion_plan(&selection, Vec::new())?;
        let command = promote_candidate_command(&first, &selection, "event-1");
        let result = ActivityResult {
            activity: CANDIDATE_PROMOTION_ACTIVITY.to_string(),
            status: ActivityStatus::Failed,
            summary: "PR creation failed".to_string(),
            artifacts: Vec::new(),
            signals: Vec::new(),
            validation: Vec::new(),
            error: Some("GitHub rejected the PR request".to_string()),
            error_kind: None,
        };

        let decision = candidate_promotion_failure_decision_inner(
            &issue_instance(),
            &event(command.clone()),
            &result,
            &command,
        )?;

        assert_eq!(decision.decision, "promote_runner_up_candidate_pr");
        assert_eq!(decision.commands.len(), 1);
        assert_eq!(
            decision.commands[0].command["candidate"]["candidate_index"],
            2
        );
        assert_eq!(
            decision.commands[0].command["promotion_chain"][0]["from_candidate_id"],
            first.selected.candidate_id
        );
        assert_eq!(
            decision.commands[0].command["promotion_chain"][0]["to_candidate_id"],
            "wf-1:candidate-group:issue-1449:c2"
        );
        let final_decision = candidate_promotion_failure_decision_inner(
            &issue_instance(),
            &event(decision.commands[0].clone()),
            &result,
            &decision.commands[0],
        )?;
        assert_eq!(final_decision.decision, "fail_candidate_promotion");
        assert_eq!(
            final_decision.commands[0].command["failure_reason"],
            final_decision.reason
        );
        assert_eq!(
            final_decision.commands[0].command["last_stop"]["state"],
            "failed"
        );
        assert_eq!(
            final_decision.commands[0].command["last_stop"]["runtime_job_id"],
            "job-1"
        );
        Ok(())
    }

    #[test]
    fn candidate_promotion_success_binds_one_pr_and_cleans_losers() -> anyhow::Result<()> {
        let selection = selection_record();
        let plan = candidate_promotion_plan(&selection, Vec::new())?;
        let command = promote_candidate_command(&plan, &selection, "event-1");
        let result = ActivityResult::succeeded(CANDIDATE_PROMOTION_ACTIVITY, "opened selected PR")
            .with_artifact(ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 1526,
                    "pr_url": "https://github.com/owner/repo/pull/1526",
                }),
            ));

        let decision = candidate_promotion_success_decision_inner(
            &issue_instance(),
            &event(command.clone()),
            &result,
            &command,
        )?;

        assert_eq!(decision.next_state, "pr_open");
        assert_eq!(
            decision
                .commands
                .iter()
                .filter(|command| command.command_type == WorkflowCommandType::BindPr)
                .count(),
            1
        );
        let cleanup = decision
            .commands
            .iter()
            .find(|command| command.command["activity"] == CANDIDATE_CLEANUP_ACTIVITY)
            .expect("cleanup command should be enqueued");
        assert_eq!(cleanup.command["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(
            cleanup.command["candidates"][0]["candidate_id"],
            "wf-1:candidate-group:issue-1449:c2"
        );
        Ok(())
    }

    #[test]
    fn deferred_candidate_result_never_binds_pr() -> anyhow::Result<()> {
        let command = WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "candidate-1",
            json!({
                "activity": "implement_issue",
                "submission_mode": "deferred",
                "candidate": {
                    "candidate_id": "wf-1:candidate-group:issue-1449:c1",
                    "candidate_index": 1,
                },
            }),
        );
        let result = ActivityResult::succeeded("implement_issue", "branch pushed").with_artifact(
            ActivityArtifact::new(
                CANDIDATE_BRANCH_ARTIFACT,
                json!({"branch": "harness/issue-1449-c1"}),
            ),
        );

        let decision = deferred_candidate_result_decision(
            &issue_instance(),
            &event(command.clone()),
            &result,
            &command,
        )
        .expect("deferred command should be handled")?;

        assert_eq!(decision.next_state, "implementing");
        assert!(decision
            .commands
            .iter()
            .all(|command| command.command_type != WorkflowCommandType::BindPr));
        Ok(())
    }
}
