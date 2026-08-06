use crate::runtime::declarative::{
    build_builtin_declarative_definition, DeclarativeWorkflowDefinition,
};
use crate::runtime::plan_issue::ISSUE_PLAN_ACTIVITY;
use crate::runtime::pr_feedback::{
    LOCAL_REVIEW_ACTIVITY, PR_FEEDBACK_DEFINITION_ID, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use crate::runtime::prompt_task::{PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY};
use crate::runtime::quality_gate::{QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID};
use crate::runtime::reducer::{
    GITHUB_ISSUE_PR_DEFINITION_ID, ISSUE_ALREADY_RESOLVED_SIGNAL, ISSUE_CLOSED_SIGNAL,
    SCOPE_TOO_LARGE_SIGNAL,
};
use crate::runtime::validator::TransitionAllowlist;
use crate::runtime::RegisteredWorkflowDefinition;
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use std::collections::BTreeMap;

pub(super) fn builtin_definitions() -> [DeclarativeWorkflowDefinition; 4] {
    [
        github_issue_pr_definition(),
        prompt_task_definition(),
        quality_gate_definition(),
        pr_feedback_definition(),
    ]
}

pub(super) fn builtin_registered_definitions() -> [RegisteredWorkflowDefinition; 4] {
    builtin_definitions().map(DeclarativeWorkflowDefinition::into_registered)
}

fn github_issue_pr_definition() -> DeclarativeWorkflowDefinition {
    use DeclaredProgressMode::{CommandDriven, ExternalWait, OperatorGate, ParentHandoff};

    let policy = WorkflowDefinitionPolicy {
        id: GITHUB_ISSUE_PR_DEFINITION_ID.to_string(),
        initial: "discovered".to_string(),
        states: BTreeMap::from([
            (
                "discovered".to_string(),
                progress(
                    CommandDriven,
                    [
                        ("DependenciesBlocked", "awaiting_dependencies"),
                        ("IssueScheduled", "scheduled"),
                        ("PlanIssue", "planning"),
                        ("SubmitImplementation", "implementing"),
                    ],
                ),
            ),
            (
                "awaiting_dependencies".to_string(),
                progress(
                    ExternalWait,
                    [
                        ("IssueScheduled", "scheduled"),
                        ("PlanIssue", "planning"),
                        ("SubmitImplementation", "implementing"),
                    ],
                ),
            ),
            (
                "scheduled".to_string(),
                progress(
                    CommandDriven,
                    [
                        ("PlanIssue", "planning"),
                        ("SubmitImplementation", "implementing"),
                        ("ReplanIssue", "replanning"),
                        ("PullRequestReady", "pr_open"),
                    ],
                ),
            ),
            (
                "planning".to_string(),
                activity(ISSUE_PLAN_ACTIVITY, Some("implementing"), []),
            ),
            (
                "implementing".to_string(),
                activity(
                    "implement_issue",
                    Some("pr_open"),
                    [
                        (ISSUE_CLOSED_SIGNAL, "done"),
                        (ISSUE_ALREADY_RESOLVED_SIGNAL, "done"),
                        (SCOPE_TOO_LARGE_SIGNAL, "blocked"),
                        ("PlanIssue", "replanning"),
                    ],
                ),
            ),
            (
                "replanning".to_string(),
                activity("replan_issue", Some("implementing"), []),
            ),
            (
                "pr_open".to_string(),
                progress(
                    ExternalWait,
                    [
                        ("LocalReviewRequested", "local_review_gate"),
                        ("AwaitFeedback", "awaiting_feedback"),
                        (ISSUE_CLOSED_SIGNAL, "done"),
                    ],
                ),
            ),
            (
                "local_review_gate".to_string(),
                activity(
                    LOCAL_REVIEW_ACTIVITY,
                    Some("awaiting_feedback"),
                    [
                        ("LocalReviewPassed", "awaiting_feedback"),
                        ("LocalReviewChangesRequested", "addressing_feedback"),
                        ("LocalReviewBlocked", "blocked"),
                    ],
                ),
            ),
            (
                "awaiting_feedback".to_string(),
                progress(
                    ExternalWait,
                    [
                        ("FeedbackFound", "addressing_feedback"),
                        ("ChangesRequested", "addressing_feedback"),
                        ("ChecksFailed", "addressing_feedback"),
                        ("PrReadyToMerge", "quality_gate_pending"),
                        (ISSUE_CLOSED_SIGNAL, "done"),
                    ],
                ),
            ),
            (
                "addressing_feedback".to_string(),
                activity("address_pr_feedback", Some("local_review_gate"), []),
            ),
            (
                "quality_gate_pending".to_string(),
                progress(
                    ParentHandoff,
                    [
                        ("QualityPassed", "ready_to_merge"),
                        (ISSUE_CLOSED_SIGNAL, "done"),
                    ],
                ),
            ),
            (
                "ready_to_merge".to_string(),
                progress(
                    OperatorGate,
                    [("MergeRequested", "merging"), (ISSUE_CLOSED_SIGNAL, "done")],
                ),
            ),
            (
                "merging".to_string(),
                activity("merge_pr", Some("done"), []),
            ),
            (
                "blocked".to_string(),
                progress(OperatorGate, std::iter::empty::<(&str, &str)>()),
            ),
        ]),
        terminal: terminal_states("done"),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec![
            "implementing".to_string(),
            "replanning".to_string(),
            "local_review_gate".to_string(),
            "awaiting_feedback".to_string(),
            "addressing_feedback".to_string(),
            "merging".to_string(),
        ],
        intake: None,
    };
    builtin(policy, TransitionAllowlist::github_issue_pr_defaults())
}

fn prompt_task_definition() -> DeclarativeWorkflowDefinition {
    use DeclaredProgressMode::{CommandDriven, ExternalWait, OperatorGate};

    let policy = WorkflowDefinitionPolicy {
        id: PROMPT_TASK_DEFINITION_ID.to_string(),
        initial: "submitted".to_string(),
        states: BTreeMap::from([
            (
                "submitted".to_string(),
                progress(
                    CommandDriven,
                    [
                        ("DependenciesBlocked", "awaiting_dependencies"),
                        ("SubmitImplementation", "implementing"),
                    ],
                ),
            ),
            (
                "awaiting_dependencies".to_string(),
                progress(ExternalWait, [("SubmitImplementation", "implementing")]),
            ),
            (
                "implementing".to_string(),
                activity(
                    PROMPT_TASK_IMPLEMENT_ACTIVITY,
                    Some("done"),
                    [
                        ("PromptContinuationActive", "implementing"),
                        (SCOPE_TOO_LARGE_SIGNAL, "blocked"),
                    ],
                ),
            ),
            (
                "blocked".to_string(),
                progress(OperatorGate, std::iter::empty::<(&str, &str)>()),
            ),
        ]),
        terminal: terminal_states("done"),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec![
            "awaiting_dependencies".to_string(),
            "implementing".to_string(),
        ],
        intake: None,
    };
    builtin(policy, TransitionAllowlist::prompt_task_defaults())
}

fn quality_gate_definition() -> DeclarativeWorkflowDefinition {
    use DeclaredProgressMode::{CommandDriven, OperatorGate};

    let policy = WorkflowDefinitionPolicy {
        id: QUALITY_GATE_DEFINITION_ID.to_string(),
        initial: "pending".to_string(),
        states: BTreeMap::from([
            (
                "pending".to_string(),
                progress(CommandDriven, [("RunQualityGate", "checking")]),
            ),
            (
                "checking".to_string(),
                activity(
                    QUALITY_GATE_ACTIVITY,
                    Some("passed"),
                    [
                        ("QualityPassed", "passed"),
                        ("QualityFailed", "failed"),
                        ("QualityBlocked", "blocked"),
                    ],
                ),
            ),
            (
                "blocked".to_string(),
                progress(OperatorGate, std::iter::empty::<(&str, &str)>()),
            ),
        ]),
        terminal: terminal_states("passed"),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec!["checking".to_string()],
        intake: None,
    };
    builtin(policy, TransitionAllowlist::quality_gate_defaults())
}

fn pr_feedback_definition() -> DeclarativeWorkflowDefinition {
    use DeclaredProgressMode::{CommandDriven, OperatorGate};

    let policy = WorkflowDefinitionPolicy {
        id: PR_FEEDBACK_DEFINITION_ID.to_string(),
        initial: "pending".to_string(),
        states: BTreeMap::from([
            (
                "pending".to_string(),
                progress(CommandDriven, [("InspectFeedback", "inspecting")]),
            ),
            (
                "inspecting".to_string(),
                activity(
                    PR_FEEDBACK_INSPECT_ACTIVITY,
                    None,
                    [
                        ("FeedbackFound", "feedback_found"),
                        ("ChangesRequested", "feedback_found"),
                        ("ChecksFailed", "feedback_found"),
                        ("NoFeedbackFound", "no_actionable_feedback"),
                        ("PrReadyToMerge", "ready_to_merge"),
                    ],
                ),
            ),
            ("feedback_found".to_string(), parent_handoff_to_done()),
            (
                "no_actionable_feedback".to_string(),
                parent_handoff_to_done(),
            ),
            ("ready_to_merge".to_string(), parent_handoff_to_done()),
            (
                "blocked".to_string(),
                progress(OperatorGate, std::iter::empty::<(&str, &str)>()),
            ),
        ]),
        terminal: terminal_states("done"),
        evidence_required: BTreeMap::new(),
        recovery_targets: vec!["inspecting".to_string()],
        intake: None,
    };
    builtin(policy, TransitionAllowlist::pr_feedback_defaults())
}

fn builtin(
    policy: WorkflowDefinitionPolicy,
    allowlist: TransitionAllowlist,
) -> DeclarativeWorkflowDefinition {
    match build_builtin_declarative_definition(&policy, &activity_policies(&policy), allowlist) {
        Ok(definition) => definition,
        Err(error) => panic!("built-in declarative workflow definition must compile: {error}"),
    }
}

fn activity(
    name: &str,
    on_success: Option<&str>,
    signals: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> DeclaredState {
    DeclaredState {
        activity: Some(name.to_string()),
        on_success: on_success.map(str::to_string),
        on_signal: signal_routes(signals),
        ..DeclaredState::default()
    }
}

fn progress(
    mode: DeclaredProgressMode,
    signals: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> DeclaredState {
    DeclaredState {
        progress: Some(mode),
        on_signal: signal_routes(signals),
        ..DeclaredState::default()
    }
}

fn parent_handoff_to_done() -> DeclaredState {
    DeclaredState {
        progress: Some(DeclaredProgressMode::ParentHandoff),
        on_success: Some("done".to_string()),
        ..DeclaredState::default()
    }
}

fn signal_routes(
    signals: impl IntoIterator<Item = (&'static str, &'static str)>,
) -> BTreeMap<String, String> {
    signals
        .into_iter()
        .map(|(signal, target)| (signal.to_string(), target.to_string()))
        .collect()
}

fn terminal_states(success_state: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("cancelled".to_string(), "cancelled".to_string()),
        ("failed".to_string(), "failed".to_string()),
        (success_state.to_string(), "succeeded".to_string()),
    ])
}

fn activity_policies(
    policy: &WorkflowDefinitionPolicy,
) -> BTreeMap<String, WorkflowActivityPolicy> {
    policy
        .states
        .values()
        .filter_map(|state| state.activity.as_ref())
        .map(|activity| (activity.clone(), WorkflowActivityPolicy::default()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::registry;
    use crate::runtime::WorkflowCommandType;
    use std::collections::BTreeSet;

    type ExpectedRule = (Option<&'static str>, &'static str, &'static [&'static str]);

    const E: &str = "enqueue_activity";
    const S: &str = "start_child_workflow";
    const B: &str = "bind_pr";
    const P: &str = "record_plan_concern";
    const W: &str = "wait";
    const MB: &str = "mark_blocked";
    const MD: &str = "mark_done";
    const MF: &str = "mark_failed";
    const MC: &str = "mark_cancelled";
    const O: &str = "request_operator_attention";

    /// The transition contract the handwritten github_issue_pr registry
    /// guaranteed before the declarative migration (including GH-1784's
    /// re-review rule). With built-ins registered declaratively this
    /// allowlist is derived from the policy above, so a missing policy edge
    /// silently withdraws a validated capability.
    const GITHUB_ISSUE_PR_RULES: &[ExpectedRule] = &[
        (Some("discovered"), "awaiting_dependencies", &[W]),
        (Some("failed"), "awaiting_dependencies", &[W]),
        (Some("cancelled"), "awaiting_dependencies", &[W]),
        (Some("awaiting_dependencies"), "awaiting_dependencies", &[W]),
        (Some("awaiting_dependencies"), "scheduled", &[E, W]),
        (Some("awaiting_dependencies"), "planning", &[E, W]),
        (Some("awaiting_dependencies"), "implementing", &[E, W]),
        (Some("discovered"), "scheduled", &[E, W]),
        (Some("discovered"), "planning", &[E, W]),
        (Some("discovered"), "implementing", &[E, W]),
        (Some("scheduled"), "scheduled", &[E, W]),
        (Some("failed"), "scheduled", &[E, W]),
        (Some("failed"), "planning", &[E, W]),
        (Some("failed"), "implementing", &[E, W]),
        (Some("failed"), "replanning", &[E, W]),
        (Some("failed"), "local_review_gate", &[E, W]),
        (Some("failed"), "awaiting_feedback", &[E, S, W]),
        (Some("failed"), "addressing_feedback", &[E, S, W]),
        (Some("failed"), "merging", &[E]),
        (Some("blocked"), "implementing", &[E, W]),
        (Some("blocked"), "replanning", &[E, W]),
        (Some("blocked"), "local_review_gate", &[E, W]),
        (Some("blocked"), "awaiting_feedback", &[E, S, W]),
        (Some("blocked"), "addressing_feedback", &[E, S, W]),
        (Some("blocked"), "merging", &[E]),
        (Some("cancelled"), "scheduled", &[E, W]),
        (Some("cancelled"), "planning", &[E, W]),
        (Some("cancelled"), "implementing", &[E, W]),
        (Some("scheduled"), "planning", &[E, W]),
        (Some("scheduled"), "implementing", &[E, P, W]),
        (Some("scheduled"), "replanning", &[E, P, MB, W]),
        (Some("planning"), "implementing", &[E, MB]),
        (Some("planning"), "planning", &[E, W]),
        (Some("implementing"), "implementing", &[E, P, W]),
        (Some("implementing"), "replanning", &[E, P, MB, W]),
        (Some("replanning"), "implementing", &[E, P, MB, W]),
        (Some("implementing"), "pr_open", &[B, E, S, W]),
        (Some("implementing"), "done", &[MD]),
        (Some("scheduled"), "pr_open", &[B, E, S, W]),
        (Some("pr_open"), "pr_open", &[B, W]),
        (Some("pr_open"), "local_review_gate", &[E, W]),
        (Some("pr_open"), "awaiting_feedback", &[W]),
        (Some("awaiting_feedback"), "local_review_gate", &[E, W]),
        (Some("local_review_gate"), "local_review_gate", &[E, W]),
        (Some("local_review_gate"), "awaiting_feedback", &[W]),
        (
            Some("local_review_gate"),
            "addressing_feedback",
            &[E, MB, W],
        ),
        (Some("pr_open"), "done", &[MD]),
        (Some("awaiting_feedback"), "awaiting_feedback", &[E, S, W]),
        (
            Some("awaiting_feedback"),
            "addressing_feedback",
            &[E, S, MB, W],
        ),
        (
            Some("addressing_feedback"),
            "addressing_feedback",
            &[E, S, MB, W],
        ),
        (Some("addressing_feedback"), "local_review_gate", &[E, S, W]),
        (Some("awaiting_feedback"), "quality_gate_pending", &[S, W]),
        (Some("quality_gate_pending"), "ready_to_merge", &[]),
        (Some("awaiting_feedback"), "done", &[MD]),
        (Some("addressing_feedback"), "done", &[MD]),
        (Some("quality_gate_pending"), "done", &[MD]),
        (Some("quality_gate_pending"), "quality_gate_pending", &[W]),
        (Some("ready_to_merge"), "ready_to_merge", &[W]),
        (Some("ready_to_merge"), "merging", &[E]),
        (Some("merging"), "done", &[MD]),
        (Some("ready_to_merge"), "done", &[MD]),
        (None, "blocked", &[MB, O, W]),
        (None, "failed", &[MF]),
        (None, "cancelled", &[MC]),
    ];

    fn command_set(names: &[&str]) -> BTreeSet<WorkflowCommandType> {
        names
            .iter()
            .map(|name| match *name {
                "enqueue_activity" => WorkflowCommandType::EnqueueActivity,
                "start_child_workflow" => WorkflowCommandType::StartChildWorkflow,
                "bind_pr" => WorkflowCommandType::BindPr,
                "record_plan_concern" => WorkflowCommandType::RecordPlanConcern,
                "wait" => WorkflowCommandType::Wait,
                "mark_blocked" => WorkflowCommandType::MarkBlocked,
                "mark_done" => WorkflowCommandType::MarkDone,
                "mark_failed" => WorkflowCommandType::MarkFailed,
                "mark_cancelled" => WorkflowCommandType::MarkCancelled,
                "request_operator_attention" => WorkflowCommandType::RequestOperatorAttention,
                other => panic!("unknown command type `{other}`"),
            })
            .collect()
    }

    #[test]
    fn github_issue_pr_builtin_preserves_the_transition_contract() {
        let definition = registry()
            .read()
            .expect("workflow definition registry lock poisoned")
            .definition(crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID)
            .expect("github_issue_pr built-in must be registered");
        let derived: Vec<(Option<String>, String, BTreeSet<WorkflowCommandType>)> = definition
            .allowlist
            .rules()
            .map(|rule| {
                (
                    rule.from_state.clone(),
                    rule.to_state.clone(),
                    rule.allowed_commands.clone(),
                )
            })
            .collect();

        let mut missing = Vec::new();
        for (from, to, commands) in GITHUB_ISSUE_PR_RULES {
            let expected = command_set(commands);
            match derived
                .iter()
                .find(|(f, t, _)| f.as_deref() == *from && t == to)
            {
                Some((_, _, actual)) if *actual == expected => {}
                Some((_, _, actual)) => missing.push(format!(
                    "{from:?} -> {to}: commands differ, expected {expected:?}, got {actual:?}"
                )),
                None => missing.push(format!("{from:?} -> {to}: rule missing")),
            }
        }
        assert!(
            missing.is_empty(),
            "contract violations:\n{}",
            missing.join("\n")
        );
    }
}
