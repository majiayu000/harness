use super::candidate_fanout::CandidateFanoutRequest;
use super::model::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvidence, WorkflowInstance,
};
use super::plan_issue::ISSUE_PLAN_ACTIVITY;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionMode {
    #[default]
    Immediate,
    Deferred,
}

impl SubmissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Deferred => "deferred",
        }
    }

    pub fn from_wire_value(value: &str) -> Option<Self> {
        match value {
            "immediate" => Some(Self::Immediate),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSubmissionWorkflowAction {
    RunPlanning,
    RunImplementation,
    WaitForDependencies,
}

#[derive(Debug, Clone)]
pub struct IssueSubmissionDecisionInput<'a> {
    pub task_id: &'a str,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub labels: &'a [String],
    pub force_execute: bool,
    pub additional_prompt: Option<&'a str>,
    pub depends_on: &'a [String],
    pub dependencies_blocked: bool,
    pub remote_fact_hash: Option<&'a str>,
    pub submission_mode: SubmissionMode,
    pub candidate_fanout: Option<CandidateFanoutRequest>,
}

#[derive(Debug, Clone)]
pub struct IssueSubmissionDecisionOutput {
    pub action: IssueSubmissionWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn build_issue_submission_decision(
    instance: &WorkflowInstance,
    input: IssueSubmissionDecisionInput<'_>,
) -> IssueSubmissionDecisionOutput {
    let action = if input.dependencies_blocked {
        IssueSubmissionWorkflowAction::WaitForDependencies
    } else if input.force_execute {
        IssueSubmissionWorkflowAction::RunImplementation
    } else {
        IssueSubmissionWorkflowAction::RunPlanning
    };
    let next_state = if input.dependencies_blocked {
        "awaiting_dependencies"
    } else if input.force_execute {
        "implementing"
    } else {
        "planning"
    };
    let reason = if input.dependencies_blocked {
        "operator submitted the GitHub issue and it is waiting for dependencies"
    } else if input.force_execute {
        "operator submitted the GitHub issue for forced implementation"
    } else {
        "operator submitted the GitHub issue for planning before implementation"
    };
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "submit_issue",
        next_state,
        reason,
    );
    if !input.dependencies_blocked && input.force_execute {
        let dedupe_key = format!("{}:{}:submit", instance.id, instance.state);
        let command = WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            dedupe_key,
            json!({
                "activity": "implement_issue",
                "additional_prompt": input.additional_prompt,
                "dispatch_gate": {
                    "reason": "uncovered_issue_ready_for_implementation",
                    "fact_hash": input.remote_fact_hash,
                },
                "remote_fact_hash": input.remote_fact_hash,
                "submission_mode": input.submission_mode.as_str(),
            }),
        );
        decision = append_candidate_commands(decision, command, input.candidate_fanout.as_ref());
    } else if !input.dependencies_blocked {
        let dedupe_key = format!("{}:{}:submit", instance.id, instance.state);
        decision = decision.with_command(WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            dedupe_key,
            json!({
                "activity": ISSUE_PLAN_ACTIVITY,
                "additional_prompt": input.additional_prompt,
                "dispatch_gate": {
                    "reason": "dependency_analysis_required",
                    "fact_hash": input.remote_fact_hash,
                },
                "remote_fact_hash": input.remote_fact_hash,
                "submission_mode": input.submission_mode.as_str(),
            }),
        ));
    }
    let decision = decision.with_evidence(WorkflowEvidence::new(
        "task_submission",
        format!(
            "task_id={} repo={} issue={} labels={} force_execute={} additional_prompt={} depends_on={} dependencies_blocked={}",
            input.task_id,
            repo_key(input.repo),
            input.issue_number,
            labels_summary(input.labels),
            input.force_execute,
            input.additional_prompt.is_some(),
            depends_on_summary(input.depends_on),
            input.dependencies_blocked
        ),
    ))
    .high_confidence();

    IssueSubmissionDecisionOutput { action, decision }
}

pub(crate) fn append_candidate_commands(
    mut decision: WorkflowDecision,
    command: WorkflowCommand,
    candidate_fanout: Option<&CandidateFanoutRequest>,
) -> WorkflowDecision {
    let Some(candidate_fanout) = candidate_fanout else {
        return decision.with_command(command);
    };
    for candidate_index in 1..=candidate_fanout.candidate_count {
        decision = decision.with_command(candidate_command(
            &command,
            candidate_fanout,
            candidate_index,
        ));
    }
    decision
}

pub(crate) fn candidate_command(
    command: &WorkflowCommand,
    candidate_fanout: &CandidateFanoutRequest,
    candidate_index: u32,
) -> WorkflowCommand {
    let mut candidate_payload = command.command.clone();
    if let Some(object) = candidate_payload.as_object_mut() {
        object.insert(
            "submission_mode".to_string(),
            json!(SubmissionMode::Deferred.as_str()),
        );
        object.insert(
            "candidate".to_string(),
            candidate_fanout.command_metadata(candidate_index),
        );
    }
    WorkflowCommand::new(
        command.command_type,
        candidate_dedupe_key(&command.dedupe_key, candidate_index),
        candidate_payload,
    )
}

pub(crate) fn candidate_dedupe_key(base: &str, candidate_index: u32) -> String {
    format!("{base}:candidate:c{candidate_index}")
}

fn labels_summary(labels: &[String]) -> String {
    if labels.is_empty() {
        return "<none>".to_string();
    }
    labels.join(",")
}

fn depends_on_summary(depends_on: &[String]) -> String {
    if depends_on.is_empty() {
        return "<none>".to_string();
    }
    depends_on.join(",")
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}
