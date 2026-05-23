use super::*;
use harness_workflow::runtime::{RuntimeKind, WorkflowSubject, REPO_BACKLOG_SPRINT_PLAN_ACTIVITY};

#[test]
fn activity_result_schema_describes_issue_implementation_terminal_evidence_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue"
        }),
    );
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
        "pr_open_with_pull_request_artifact_or_done_with_closed_issue_signal_else_blocked"
    );
    assert_eq!(
        schema["activity_contract"]["accepted_signals"][0],
        ISSUE_CLOSED_SIGNAL
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "pull_request_artifact_or_closed_issue_signal"
    );
    assert_eq!(
        schema["agent_summary_contract"]["artifacts"]["issue_state"]["fields"][1],
        "state"
    );
    assert_eq!(
            schema["agent_summary_contract"]["signals"]["IssueAlreadyResolved"],
            "Use when the task is already resolved before a PR is created. Include state=closed or state=resolved plus issue_number or issue_url."
        );
}

#[test]
fn activity_result_schema_describes_quality_gate_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": QUALITY_GATE_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        "checking",
        WorkflowSubject::new("quality_gate", "issue:123"),
    )
    .with_id("quality-gate-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], QUALITY_GATE_DEFINITION_ID);
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
        "passed"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["output_signal"],
        QUALITY_PASSED_SIGNAL
    );
    assert_eq!(
        schema["agent_summary_contract"]["must_include"][0],
        "validation commands"
    );
    assert_eq!(
        schema["activity_contract"]["accepted_signals"][0],
        QUALITY_PASSED_SIGNAL
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "quality_gate_status_signal_and_validation_evidence"
    );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "passed"));
}

#[test]
fn activity_result_schema_describes_repo_backlog_poll_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "poll_repo_backlog"
        }),
    );
    let workflow = WorkflowInstance::new(
        "repo_backlog",
        1,
        "scanning",
        WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], "repo_backlog");
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        "IssueDiscovered"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["empty_success_allowed"],
        false
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "at_least_one_accepted_signal"
    );
    assert_eq!(
        schema["activity_contract"]["explicit_noop_signals"][0],
        "NoOpenIssueFound"
    );
    assert_eq!(
            schema["agent_summary_contract"]["signals"]["IssueDiscovered"],
            "Use for each open GitHub issue that should be considered by the runtime sprint planner. Include issue_number, issue_url, repo, title, and labels when available."
        );
    assert_eq!(
            schema["agent_summary_contract"]["success_rule"],
            "A succeeded result MUST emit at least one accepted issue or open PR feedback signal. Do not return succeeded with empty signals."
        );
    assert_eq!(
            schema["agent_summary_contract"]["signals"]["OpenPrFeedbackDiscovered"],
            "Use for each open PR with unresolved actionable review feedback and no active bound workflow. Include pr_number, pr_url, repo, title, feedback_count, and summary."
        );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "planning_batch"));
}

#[test]
fn activity_result_schema_describes_repo_sprint_plan_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": REPO_BACKLOG_SPRINT_PLAN_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        "repo_backlog",
        1,
        "planning_batch",
        WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], "repo_backlog");
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        "SprintTaskSelected"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["empty_success_allowed"],
        false
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_artifacts"][0],
        "sprint_plan"
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "at_least_one_accepted_signal_or_artifact"
    );
    assert_eq!(
        schema["activity_contract"]["accepted_artifacts"][0],
        "sprint_plan"
    );
    assert_eq!(
            schema["agent_summary_contract"]["signals"]["SprintTaskSelected"],
            "Use once for each issue selected for execution. Include issue_number, issue_url, repo, labels, and depends_on as issue numbers."
        );
    assert_eq!(
            schema["agent_summary_contract"]["success_rule"],
            "A succeeded result MUST emit SprintTaskSelected, IssueSkipped, NoSprintTaskSelected, or a sprint_plan artifact. Do not return succeeded with empty signals and no sprint_plan artifact."
        );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "dispatching"));
    let command = &schema["wire_format_example"]["artifacts"][0]["artifact"]["commands"][0];
    assert_eq!(command["command_type"], "start_child_workflow");
    assert_eq!(command["command"]["definition_id"], "github_issue_pr");
    assert_eq!(command["command"]["subject_key"], "issue:42");
    assert_eq!(command["command"]["repo"], "owner/repo");
    assert_eq!(command["command"]["issue_number"], 42);
}

#[test]
fn activity_result_schema_describes_pr_feedback_child_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": PR_FEEDBACK_INSPECT_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("pr-feedback-1");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(schema["workflow_definition"], PR_FEEDBACK_DEFINITION_ID);
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        "FeedbackFound"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["parent_propagation"],
        "The same activity result is propagated to the parent github_issue_pr workflow."
    );
    assert_eq!(
        schema["activity_contract"]["child_outcome_contract"],
        "pr_feedback_outcome"
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["PrReadyToMerge"],
        "Use only when review, checks, and mergeability are all ready."
    );
    assert!(schema["workflow_decision_contract"]["allowed_transitions"]
        .as_array()
        .expect("allowed transitions should be an array")
        .iter()
        .any(|transition| transition["next_state"] == "feedback_found"));
}

#[test]
fn runtime_prompt_packet_includes_workflow_file_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "runtime_profile": RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc)
        }),
    );
    let workflow_document = WorkflowDocument {
        prompt_template: "Follow the repository workflow prompt.".to_string(),
        source_path: Some("/repo/WORKFLOW.md".to_string()),
        ..Default::default()
    };
    let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

    let packet = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &runtime_profile,
        &workflow_document,
    );
    assert_eq!(packet["project"]["root"], "/workspaces/job-1");
    assert_eq!(packet["project"]["source_root"], "/repo");
    assert_eq!(
        packet["workflow_file"]["prompt_template"],
        "Follow the repository workflow prompt."
    );

    let prompt = build_runtime_job_prompt(&packet, None);
    assert!(prompt.contains("Repository workflow prompt template:"));
    assert!(prompt.contains("Follow the repository workflow prompt."));
}
