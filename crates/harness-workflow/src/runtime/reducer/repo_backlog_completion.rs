use super::repo_backlog_candidates::{
    issue_candidates_from_signals, known_dependency_candidates_from_skipped_signals,
    normalize_selected_sprint_dependencies, pr_feedback_candidates_from_signals,
    sprint_plan_known_dependency_inputs, sprint_plan_pr_feedback_inputs,
    sprint_plan_selected_issues, RepoBacklogIssueCandidate, RepoBacklogPrFeedbackCandidate,
};
use super::support::{
    array_items, event_command_type, has_any_signal, invalid_agent_output_blocked_decision,
    json_value_u64, non_empty_json_string, optional_data_string, runtime_completion_evidence,
    signal_count,
};
use super::GITHUB_ISSUE_PR_DEFINITION_ID;
use crate::runtime::model::{
    ActivityResult, WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvent,
    WorkflowInstance,
};
use crate::runtime::prompt_task::PROMPT_TASK_DEFINITION_ID;
use crate::runtime::repo_backlog::{
    REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
};
use crate::runtime::validator::{DecisionValidator, ValidationContext};
use chrono::Utc;
use serde_json::{json, Value};

pub(super) fn repo_backlog_poll_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        REPO_BACKLOG_DEFINITION_ID,
        "scanning",
        REPO_BACKLOG_POLL_ACTIVITY,
    ) {
        return None;
    }

    let parent_repo = optional_data_string(instance, "repo");
    let discovered =
        issue_candidates_from_signals(result, "IssueDiscovered", parent_repo.as_deref());
    let pr_feedback = pr_feedback_candidates_from_signals(
        result,
        "OpenPrFeedbackDiscovered",
        parent_repo.as_deref(),
    );
    let known_dependencies =
        known_dependency_candidates_from_skipped_signals(result, parent_repo.as_deref());

    if discovered.is_empty() && pr_feedback.is_empty() {
        return None;
    }

    if discovered.is_empty() {
        let mut decision = WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "start_open_pr_feedback_tasks_from_scan",
            "dispatching",
            format!(
                "repo backlog scan discovered {} open PR(s) with actionable review feedback",
                pr_feedback.len()
            ),
        )
        .with_evidence(runtime_completion_evidence(event, result));

        for candidate in pr_feedback {
            decision = decision
                .with_command(candidate.start_prompt_task_command(event))
                .with_evidence(candidate.github_pr_evidence());
        }

        return Some(decision.high_confidence());
    }

    let issue_payloads = discovered
        .iter()
        .map(RepoBacklogIssueCandidate::to_command_value)
        .collect::<Vec<_>>();
    let pr_feedback_payloads = pr_feedback
        .iter()
        .map(RepoBacklogPrFeedbackCandidate::to_command_value)
        .collect::<Vec<_>>();
    let known_dependency_payloads = known_dependencies
        .iter()
        .map(RepoBacklogIssueCandidate::to_command_value)
        .collect::<Vec<_>>();
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "plan_repo_sprint_from_scan",
        "planning_batch",
        format!(
            "repo backlog scan discovered {} open issue(s); runtime sprint planning must select dispatch order",
            discovered.len()
        ),
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!("runtime-completion:{}:plan-repo-sprint", event.id),
        json!({
            "activity": REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
            "repo": parent_repo,
            "issues": issue_payloads,
            "open_pr_feedback": pr_feedback_payloads,
            "known_dependencies": known_dependency_payloads,
        }),
    ))
    .with_evidence(runtime_completion_evidence(event, result));

    for candidate in discovered {
        decision = decision.with_evidence(candidate.github_issue_evidence());
    }
    for candidate in pr_feedback {
        decision = decision.with_evidence(candidate.github_pr_evidence());
    }

    Some(decision.high_confidence())
}

pub(super) fn repo_backlog_sprint_plan_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> Option<WorkflowDecision> {
    if (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) != (
        REPO_BACKLOG_DEFINITION_ID,
        "planning_batch",
        REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
    ) {
        return None;
    }

    let parent_repo = optional_data_string(instance, "repo");
    let selected = normalize_selected_sprint_dependencies(
        sprint_plan_selected_issues(result, event, parent_repo.as_deref()),
        &sprint_plan_known_dependency_inputs(event, parent_repo.as_deref()),
    );
    let pr_feedback = sprint_plan_pr_feedback_inputs(event, parent_repo.as_deref());
    if selected.is_empty() && pr_feedback.is_empty() {
        return None;
    }

    let decision_name = if pr_feedback.is_empty() {
        "start_issue_workflows_from_sprint_plan"
    } else {
        "start_repo_backlog_workflows_from_sprint_plan"
    };
    let reason = if pr_feedback.is_empty() {
        format!(
            "runtime sprint planner selected {} issue workflow(s) for dispatch",
            selected.len()
        )
    } else {
        format!(
            "runtime sprint planner selected {} issue workflow(s) and {} open PR feedback task(s) for dispatch",
            selected.len(),
            pr_feedback.len()
        )
    };
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        "dispatching",
        reason,
    )
    .with_evidence(runtime_completion_evidence(event, result));

    for candidate in selected {
        let issue_number = candidate.issue_number;
        let repo_key = candidate.repo.as_deref().unwrap_or("<none>");
        decision = decision
            .with_command(WorkflowCommand::new(
                WorkflowCommandType::StartChildWorkflow,
                format!("repo-sprint-plan:{repo_key}:issue:{issue_number}:start"),
                json!({
                    "definition_id": "github_issue_pr",
                    "subject_key": format!("issue:{issue_number}"),
                    "repo": candidate.repo,
                    "issue_number": issue_number,
                    "issue_url": candidate.issue_url,
                    "title": candidate.title,
                    "labels": candidate.labels,
                    "depends_on": candidate.depends_on,
                    "source": "github",
                    "external_id": issue_number.to_string(),
                    "auto_submit": true,
                }),
            ))
            .with_evidence(candidate.github_issue_evidence());
    }
    for candidate in pr_feedback {
        decision = decision
            .with_command(candidate.start_prompt_task_command(event))
            .with_evidence(candidate.github_pr_evidence());
    }

    Some(decision.high_confidence())
}

pub(super) fn repo_backlog_invalid_success_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    structured_decision: Option<&WorkflowDecision>,
) -> Option<WorkflowDecision> {
    match (
        instance.definition_id.as_str(),
        instance.state.as_str(),
        result.activity.as_str(),
    ) {
        (REPO_BACKLOG_DEFINITION_ID, "scanning", REPO_BACKLOG_POLL_ACTIVITY) => {
            let issue_discovered_count = signal_count(result, "IssueDiscovered");
            let valid_issue_discovered_count =
                issue_candidates_from_signals(result, "IssueDiscovered", None).len();
            if issue_discovered_count > 0 && valid_issue_discovered_count == 0 {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo backlog poll emitted IssueDiscovered signals, but none contained a valid issue_number",
                ));
            }
            let pr_feedback_count = signal_count(result, "OpenPrFeedbackDiscovered");
            let valid_pr_feedback_count =
                pr_feedback_candidates_from_signals(result, "OpenPrFeedbackDiscovered", None).len();
            if pr_feedback_count > 0 && valid_pr_feedback_count == 0 {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo backlog poll emitted OpenPrFeedbackDiscovered signals, but none contained a valid pr_number",
                ));
            }
            if !has_any_signal(
                result,
                &[
                    "IssueDiscovered",
                    "IssueSkipped",
                    "NoOpenIssueFound",
                    "OpenPrFeedbackDiscovered",
                    "OpenPrFeedbackSkipped",
                    "NoOpenPrFeedbackFound",
                ],
            ) {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo backlog poll succeeded without issue or open PR feedback signals",
                ));
            }
            if structured_decision.is_some() {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo backlog poll emitted a workflow_decision artifact that did not validate and no valid domain fallback was available",
                ));
            }
            None
        }
        (REPO_BACKLOG_DEFINITION_ID, "planning_batch", REPO_BACKLOG_SPRINT_PLAN_ACTIVITY) => {
            let sprint_task_selected_count = signal_count(result, "SprintTaskSelected");
            let sprint_plan_task_count = sprint_plan_task_count(result);
            let has_valid_noop_sprint_plan_artifact = has_valid_noop_sprint_plan_artifact(result);
            if (sprint_task_selected_count > 0 || sprint_plan_task_count > 0)
                && sprint_plan_selected_issues(result, event, None).is_empty()
            {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo sprint plan emitted selected task output, but no selected issue had a valid issue_number",
                ));
            }
            if !has_any_signal(
                result,
                &["SprintTaskSelected", "IssueSkipped", "NoSprintTaskSelected"],
            ) && sprint_plan_task_count == 0
                && !has_valid_noop_sprint_plan_artifact
            {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo sprint plan succeeded without SprintTaskSelected, IssueSkipped, NoSprintTaskSelected, or a valid no-op sprint_plan artifact",
                ));
            }
            if structured_decision.is_some() {
                return Some(invalid_agent_output_blocked_decision(
                    instance,
                    event,
                    result,
                    "repo sprint plan emitted a workflow_decision artifact that did not validate and no valid domain fallback was available",
                ));
            }
            None
        }
        _ => None,
    }
}

const LEGACY_REPO_BACKLOG_SCAN_PLAN_ACTIVITY: &str = "plan_repo_sprint_from_scan";
const LEGACY_REPO_BACKLOG_SCAN_DISPATCH_DECISION: &str = "start_repo_backlog_workflows_from_scan";

pub(super) fn repo_backlog_legacy_scan_dispatch_decision_from_activity_result(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    structured_decision: Option<&WorkflowDecision>,
) -> Option<WorkflowDecision> {
    if instance.definition_id != REPO_BACKLOG_DEFINITION_ID {
        return None;
    }
    if !matches!(instance.state.as_str(), "scanning" | "planning_batch") {
        return None;
    }
    if result.activity != LEGACY_REPO_BACKLOG_SCAN_PLAN_ACTIVITY {
        return None;
    }

    let decision = structured_decision?;
    if decision.decision != LEGACY_REPO_BACKLOG_SCAN_DISPATCH_DECISION {
        return None;
    }
    if decision.next_state != "dispatching" {
        return None;
    }
    if decision.commands.is_empty()
        || !decision
            .commands
            .iter()
            .all(valid_legacy_repo_backlog_dispatch_command)
    {
        return None;
    }

    let decision_name = if decision
        .commands
        .iter()
        .any(|command| start_child_definition_id(command) == Some(PROMPT_TASK_DEFINITION_ID))
    {
        "start_repo_backlog_workflows_from_sprint_plan"
    } else {
        "start_issue_workflows_from_sprint_plan"
    };
    let mut normalized = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        "dispatching",
        "repo backlog accepted legacy scan dispatch output as a sprint plan decision",
    )
    .high_confidence();
    normalized.commands = decision.commands.clone();
    normalized.evidence = decision.evidence.clone();
    if normalized.evidence.is_empty() {
        normalized
            .evidence
            .push(runtime_completion_evidence(event, result));
    }

    DecisionValidator::repo_backlog()
        .validate(
            instance,
            &normalized,
            &ValidationContext::new(event.source.as_str(), Utc::now()),
        )
        .is_ok()
        .then_some(normalized)
}

fn valid_legacy_repo_backlog_dispatch_command(command: &WorkflowCommand) -> bool {
    if command.command_type != WorkflowCommandType::StartChildWorkflow {
        return false;
    }

    let Some(subject_key) = command.command.get("subject_key").and_then(Value::as_str) else {
        return false;
    };
    match start_child_definition_id(command) {
        Some(GITHUB_ISSUE_PR_DEFINITION_ID) => subject_key.starts_with("issue:"),
        Some(PROMPT_TASK_DEFINITION_ID) => {
            subject_key.starts_with("pr:") && subject_key.ends_with(":feedback")
        }
        _ => false,
    }
}

fn start_child_definition_id(command: &WorkflowCommand) -> Option<&str> {
    if command.command_type != WorkflowCommandType::StartChildWorkflow {
        return None;
    }
    command.command.get("definition_id").and_then(Value::as_str)
}

fn sprint_plan_task_count(result: &ActivityResult) -> usize {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "sprint_plan")
        .map(|artifact| array_items(&artifact.artifact, "tasks").len())
        .sum()
}

fn has_valid_noop_sprint_plan_artifact(result: &ActivityResult) -> bool {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "sprint_plan")
        .any(|artifact| valid_noop_sprint_plan_artifact(&artifact.artifact))
}

fn valid_noop_sprint_plan_artifact(value: &Value) -> bool {
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return false;
    };
    tasks.is_empty() && (has_valid_skip_evidence(value) || has_explicit_noop_evidence(value))
}

fn has_valid_skip_evidence(value: &Value) -> bool {
    value
        .get("skip")
        .or_else(|| value.get("skipped"))
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(valid_sprint_plan_skip_evidence))
}

fn valid_sprint_plan_skip_evidence(value: &Value) -> bool {
    let has_issue = value
        .get("issue")
        .or_else(|| value.get("issue_number"))
        .and_then(json_value_u64)
        .is_some_and(|issue_number| issue_number > 0);
    let has_reason = value
        .get("reason")
        .or_else(|| value.get("note"))
        .and_then(non_empty_json_string)
        .is_some();
    has_issue && has_reason
}

fn has_explicit_noop_evidence(value: &Value) -> bool {
    let has_noop_flag = value
        .get("no_task_selected")
        .or_else(|| value.get("no_tasks_selected"))
        .or_else(|| value.get("no_sprint_task_selected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_reason = value
        .get("reason")
        .or_else(|| value.get("summary"))
        .or_else(|| value.get("note"))
        .and_then(non_empty_json_string)
        .is_some();
    has_noop_flag && has_reason
}

pub(super) fn repo_backlog_child_dispatch_still_active(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
) -> bool {
    instance.definition_id == REPO_BACKLOG_DEFINITION_ID
        && instance.state == "dispatching"
        && event_command_type(event) == Some("start_child_workflow")
        && event
            .event
            .get("active_start_child_workflow_commands")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{
        ActivityArtifact, WorkflowCommand, WorkflowCommandType, WorkflowSubject,
    };
    use crate::runtime::reducer::{reduce_runtime_job_completed, RUNTIME_JOB_COMPLETED_EVENT};
    use serde_json::json;

    fn repo_backlog_instance(state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            REPO_BACKLOG_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("repo", "owner/repo"),
        )
    }

    #[test]
    fn legacy_scan_dispatch_workflow_decision_is_normalized() {
        let instance = repo_backlog_instance("scanning");
        let command = WorkflowCommand::enqueue_activity(
            LEGACY_REPO_BACKLOG_SCAN_PLAN_ACTIVITY,
            "repo-backlog:owner/repo:legacy-plan",
        );
        let workflow_decision = WorkflowDecision::new(
            &instance.id,
            "planning_batch",
            LEGACY_REPO_BACKLOG_SCAN_DISPATCH_DECISION,
            "dispatching",
            "legacy runtime selected implementation and PR feedback work",
        )
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::StartChildWorkflow,
            "repo-sprint-plan:owner/repo:issue:1165:start",
            json!({
                "definition_id": GITHUB_ISSUE_PR_DEFINITION_ID,
                "subject_key": "issue:1165",
                "repo": "owner/repo",
                "issue_number": 1165,
                "source": "github",
                "external_id": "1165",
                "auto_submit": true,
            }),
        ))
        .with_command(WorkflowCommand::new(
            WorkflowCommandType::StartChildWorkflow,
            "repo-backlog:owner/repo:pr:589:feedback",
            json!({
                "definition_id": PROMPT_TASK_DEFINITION_ID,
                "subject_key": "pr:589:feedback",
                "repo": "owner/repo",
                "pr_number": 589,
                "source": "github_pr_feedback",
                "external_id": "pr-feedback:owner/repo:589",
                "task_id": "repo-backlog:owner/repo:pr:589:feedback",
                "prompt": "Handle unresolved review feedback for PR owner/repo#589.",
            }),
        ))
        .high_confidence();
        let result = ActivityResult::succeeded(
            LEGACY_REPO_BACKLOG_SCAN_PLAN_ACTIVITY,
            "Selected issue and PR feedback work for dispatch.",
        )
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(workflow_decision).expect("decision should serialize"),
        ));
        let event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
            .with_payload(json!({
                "command_id": "command-1",
                "command": command,
                "runtime_job_id": "job-1",
                "activity_result": result,
            }));

        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("event should parse")
            .expect("legacy dispatch output should reduce to a workflow decision");

        assert_eq!(
            decision.decision,
            "start_repo_backlog_workflows_from_sprint_plan"
        );
        assert_eq!(decision.observed_state, "scanning");
        assert_eq!(decision.next_state, "dispatching");
        assert_eq!(decision.commands.len(), 2);
        assert_eq!(
            decision.commands[0].command["definition_id"],
            GITHUB_ISSUE_PR_DEFINITION_ID
        );
        assert_eq!(
            decision.commands[1].command["definition_id"],
            PROMPT_TASK_DEFINITION_ID
        );
        DecisionValidator::repo_backlog()
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime-1", Utc::now()),
            )
            .expect("normalized legacy repo backlog dispatch should validate");
    }

    #[test]
    fn legacy_scan_dispatch_without_child_commands_still_blocks() {
        let instance = repo_backlog_instance("scanning");
        let command = WorkflowCommand::enqueue_activity(
            LEGACY_REPO_BACKLOG_SCAN_PLAN_ACTIVITY,
            "repo-backlog:owner/repo:legacy-plan",
        );
        let workflow_decision = WorkflowDecision::new(
            &instance.id,
            "planning_batch",
            LEGACY_REPO_BACKLOG_SCAN_DISPATCH_DECISION,
            "dispatching",
            "legacy runtime selected no child workflows",
        )
        .high_confidence();
        let result = ActivityResult::succeeded(
            LEGACY_REPO_BACKLOG_SCAN_PLAN_ACTIVITY,
            "Returned an invalid legacy dispatch decision.",
        )
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(workflow_decision).expect("decision should serialize"),
        ));
        let event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
            .with_payload(json!({
                "command_id": "command-1",
                "command": command,
                "runtime_job_id": "job-1",
                "activity_result": result,
            }));

        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("event should parse")
            .expect("invalid legacy dispatch output should block");

        assert_eq!(decision.decision, "block_invalid_agent_output");
        assert_eq!(decision.next_state, "blocked");
        assert!(decision.reason.contains("no domain fallback was available"));
        assert!(!decision
            .commands
            .iter()
            .any(|command| command.command_type == WorkflowCommandType::StartChildWorkflow));
        DecisionValidator::repo_backlog()
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime-1", Utc::now()),
            )
            .expect("invalid legacy dispatch block should validate");
    }
}
