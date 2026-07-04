use super::model::{WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance};
use super::plan_issue::ISSUE_PLAN_ACTIVITY;
use super::remote_facts::remote_fact_command_dedupe_key;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy)]
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
        let dedupe_key = submission_command_dedupe_key(
            "implement_issue",
            input.remote_fact_hash,
            format!(
                "issue-submit:{}:issue:{}:task:{}:implement",
                repo_key(input.repo),
                input.issue_number,
                input.task_id
            ),
        );
        decision = decision.with_command(WorkflowCommand::new(
            super::model::WorkflowCommandType::EnqueueActivity,
            dedupe_key,
            serde_json::json!({
                "activity": "implement_issue",
                "additional_prompt": input.additional_prompt,
                "dispatch_gate": {
                    "reason": "uncovered_issue_ready_for_implementation",
                    "fact_hash": input.remote_fact_hash,
                },
                "remote_fact_hash": input.remote_fact_hash,
                "submission_mode": input.submission_mode.as_str(),
            }),
        ));
    } else if !input.dependencies_blocked {
        let dedupe_key = submission_command_dedupe_key(
            ISSUE_PLAN_ACTIVITY,
            input.remote_fact_hash,
            format!(
                "issue-submit:{}:issue:{}:task:{}:plan",
                repo_key(input.repo),
                input.issue_number,
                input.task_id
            ),
        );
        decision = decision.with_command(WorkflowCommand::new(
            super::model::WorkflowCommandType::EnqueueActivity,
            dedupe_key,
            serde_json::json!({
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

fn submission_command_dedupe_key(
    activity: &str,
    remote_fact_hash: Option<&str>,
    fallback: String,
) -> String {
    remote_fact_hash
        .map(|fact_hash| remote_fact_command_dedupe_key(activity, fact_hash))
        .unwrap_or(fallback)
}
