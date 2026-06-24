use harness_workflow::runtime::{
    GITHUB_ISSUE_PR_DEFINITION_ID, ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL,
    ISSUE_PLAN_ACTIVITY, ISSUE_PLAN_ARTIFACT, ISSUE_PLAN_READY_SIGNAL, ISSUE_STATE_ARTIFACT,
    PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY, PR_FEEDBACK_SNAPSHOT_ARTIFACT, PR_REPAIR_SNAPSHOT_ARTIFACT,
    QUALITY_BLOCKED_SIGNAL, QUALITY_FAILED_SIGNAL, QUALITY_GATE_ACTIVITY,
    QUALITY_GATE_DEFINITION_ID, QUALITY_PASSED_SIGNAL, SCOPE_TOO_LARGE_SIGNAL,
    SERVER_PR_SNAPSHOT_ARTIFACT,
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
        (PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => {
            pr_feedback_contract(workflow_definition, activity)
                .with_child_outcome("pr_feedback_outcome")
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![
                    harness_workflow::runtime::LOCAL_REVIEW_PASSED_SIGNAL,
                    harness_workflow::runtime::LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL,
                    harness_workflow::runtime::LOCAL_REVIEW_BLOCKED_SIGNAL,
                ])
                .requires("at_least_one_local_review_outcome_signal")
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "sweep_pr_feedback")
        | (GITHUB_ISSUE_PR_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY) => {
            pr_feedback_contract(workflow_definition, activity)
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "implement_issue") => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![
                    ISSUE_CLOSED_SIGNAL,
                    ISSUE_ALREADY_RESOLVED_SIGNAL,
                    SCOPE_TOO_LARGE_SIGNAL,
                ])
                .with_accepted_artifacts(vec![
                    "pull_request",
                    ISSUE_STATE_ARTIFACT,
                    "workflow_decision",
                ])
                .requires("pull_request_artifact_or_closed_issue_or_scope_too_large_signal")
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, ISSUE_PLAN_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_signals(vec![ISSUE_PLAN_READY_SIGNAL])
                .with_accepted_artifacts(vec![ISSUE_PLAN_ARTIFACT])
                .requires("issue_plan_artifact_or_ready_signal")
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "replan_issue") => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_artifacts(vec!["workflow_decision"])
        }
        (GITHUB_ISSUE_PR_DEFINITION_ID, "address_pr_feedback") => ActivityContract::new(
            workflow_definition,
            activity,
        )
        .with_accepted_signals(vec![ISSUE_CLOSED_SIGNAL, ISSUE_ALREADY_RESOLVED_SIGNAL])
        .with_accepted_artifacts(vec![
            "workflow_decision",
            PR_REPAIR_SNAPSHOT_ARTIFACT,
            ISSUE_STATE_ARTIFACT,
        ])
        .requires("pr_repair_snapshot_with_action_and_passing_validation_or_closed_issue_evidence"),
        (PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY) => {
            ActivityContract::new(workflow_definition, activity)
                .with_accepted_artifacts(vec!["validation_report"])
                .requires("validation_evidence")
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
        .with_accepted_artifacts(vec![
            "workflow_decision",
            SERVER_PR_SNAPSHOT_ARTIFACT,
            PR_FEEDBACK_SNAPSHOT_ARTIFACT,
        ])
        .with_explicit_noop_signals(vec!["NoFeedbackFound"])
        .requires("at_least_one_feedback_outcome_signal; ready_to_merge_requires_current_pr_readiness_snapshot")
}
