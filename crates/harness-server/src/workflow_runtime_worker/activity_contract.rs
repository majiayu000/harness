use harness_workflow::runtime::{
    GITHUB_ISSUE_PR_DEFINITION_ID, ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL,
    ISSUE_STATE_ARTIFACT, PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY,
    PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY, QUALITY_BLOCKED_SIGNAL,
    QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID,
    QUALITY_PASSED_SIGNAL, REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY,
    REPO_BACKLOG_SPRINT_PLAN_ACTIVITY,
};
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ActivityContract {
    pub schema: &'static str,
    pub workflow_definition: String,
    pub activity: String,
    pub accepted_signals: Vec<&'static str>,
    pub accepted_artifacts: Vec<&'static str>,
    pub explicit_noop_signals: Vec<&'static str>,
    pub required_artifacts: Vec<&'static str>,
    pub empty_success_allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_requires: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_outcome_contract: Option<&'static str>,
}

impl ActivityContract {
    fn new(workflow_definition: &str, activity: &str) -> Self {
        Self {
            schema: "harness.runtime.activity_contract.v1",
            workflow_definition: workflow_definition.to_string(),
            activity: activity.to_string(),
            accepted_signals: Vec::new(),
            accepted_artifacts: Vec::new(),
            explicit_noop_signals: Vec::new(),
            required_artifacts: Vec::new(),
            empty_success_allowed: true,
            success_requires: None,
            child_outcome_contract: None,
        }
    }

    fn with_accepted_signals(mut self, signals: Vec<&'static str>) -> Self {
        self.accepted_signals = signals;
        self
    }

    fn with_accepted_artifacts(mut self, artifacts: Vec<&'static str>) -> Self {
        self.accepted_artifacts = artifacts;
        self
    }

    fn with_explicit_noop_signals(mut self, signals: Vec<&'static str>) -> Self {
        self.explicit_noop_signals = signals;
        self
    }

    fn with_required_artifacts(mut self, artifacts: Vec<&'static str>) -> Self {
        self.required_artifacts = artifacts;
        self
    }

    fn requires(mut self, requirement: &'static str) -> Self {
        self.empty_success_allowed = false;
        self.success_requires = Some(requirement);
        self
    }

    fn with_child_outcome(mut self, contract: &'static str) -> Self {
        self.child_outcome_contract = Some(contract);
        self
    }

    pub(super) fn to_prompt_value(&self) -> Value {
        json!(self)
    }
}

pub(super) fn activity_contract(workflow_definition: &str, activity: &str) -> ActivityContract {
    match (workflow_definition, activity) {
        (REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_POLL_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![
                    "IssueDiscovered",
                    "IssueSkipped",
                    "NoOpenIssueFound",
                    "OpenPrFeedbackDiscovered",
                    "OpenPrFeedbackSkipped",
                    "NoOpenPrFeedbackFound",
                ])
                .with_explicit_noop_signals(vec!["NoOpenIssueFound", "NoOpenPrFeedbackFound"])
                .requires("at_least_one_accepted_signal")
        }
        (REPO_BACKLOG_DEFINITION_ID, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![
                    "SprintTaskSelected",
                    "IssueSkipped",
                    "NoSprintTaskSelected",
                ])
                .with_accepted_artifacts(vec!["sprint_plan"])
                .with_explicit_noop_signals(vec!["NoSprintTaskSelected"])
                .requires("at_least_one_accepted_signal_or_artifact")
        }
        (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => {
            pr_feedback_contract(workflow_definition, activity)
                .with_child_outcome("pr_feedback_outcome")
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "sweep_pr_feedback")
        | (GITHUB_ISSUE_PR_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => {
            pr_feedback_contract(workflow_definition, activity)
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "implement_issue") => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![ISSUE_CLOSED_SIGNAL, ISSUE_ALREADY_RESOLVED_SIGNAL])
                .with_accepted_artifacts(vec![
                    "pull_request",
                    ISSUE_STATE_ARTIFACT,
                    "workflow_decision",
                ])
                .requires("pull_request_artifact_or_closed_issue_signal")
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "replan_issue") => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_artifacts(vec!["workflow_decision"])
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "address_pr_feedback") => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_artifacts(vec!["workflow_decision"])
        }
        (PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_artifacts(vec!["validation_report"])
        }
        (QUALITY_GATE_DEFINITION_ID, QUALITY_GATE_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![
                    QUALITY_PASSED_SIGNAL,
                    QUALITY_FAILED_SIGNAL,
                    QUALITY_BLOCKED_SIGNAL,
                ])
                .with_accepted_artifacts(vec!["validation_report"])
                .requires("quality_gate_status_signal_and_validation_evidence")
        }
        (REPO_BACKLOG_DEFINITION_ID, "start_child_workflow") => {
            ActivityContract::new(workflow_definition, activity)
                .with_required_artifacts(vec!["child_workflow"])
                .requires("child_workflow_artifact")
        }
        (REPO_BACKLOG_DEFINITION_ID, "mark_bound_issue_done")
        | (REPO_BACKLOG_DEFINITION_ID, "recover_issue_workflow") => {
            ActivityContract::new(workflow_definition, activity)
                .with_required_artifacts(vec!["child_workflow"])
                .requires("child_workflow_artifact")
        }
        _ => ActivityContract::new(workflow_definition, activity),
    }
}

fn pr_feedback_contract(workflow_definition: &str, activity: &str) -> ActivityContract {
    ActivityContract::new(workflow_definition, activity)
        .with_accepted_signals(vec![
            "FeedbackFound",
            "NoFeedbackFound",
            "PrReadyToMerge",
            "ChangesRequested",
            "ChecksFailed",
        ])
        .with_explicit_noop_signals(vec!["NoFeedbackFound"])
        .requires("at_least_one_feedback_outcome_signal")
}
