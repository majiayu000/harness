use super::*;
use harness_workflow::runtime::{
    RuntimeKind, WorkflowSubject, ISSUE_PLAN_ACTIVITY, ISSUE_PLAN_ARTIFACT,
    ISSUE_PLAN_READY_SIGNAL, PR_REPAIR_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ARTIFACT,
};

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
        "pr_open_with_pull_request_artifact_or_done_with_closed_issue_signal_or_blocked_with_scope_too_large_signal_else_blocked"
    );
    assert_eq!(
        schema["activity_contract"]["accepted_signals"][0],
        ISSUE_CLOSED_SIGNAL
    );
    assert_eq!(
        schema["activity_contract"]["accepted_signals"][2],
        SCOPE_TOO_LARGE_SIGNAL
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "pull_request_artifact_or_closed_issue_or_scope_too_large_signal"
    );
    assert_eq!(
        schema["agent_summary_contract"]["artifacts"]["issue_state"]["fields"][1],
        "state"
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["IssueAlreadyResolved"],
        "Use when the task is already resolved before a PR is created. Include state=closed or state=resolved plus issue_number or issue_url."
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["pr_scope_guard"]["threshold_config"],
        "workflow_file.config.pr_scope_guard; defaults are enabled=true, max_files_changed=30, max_lines_added=1500."
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["SCOPE_TOO_LARGE"],
        "Use when pr_scope_guard is enabled and the diff exceeds configured max_files_changed or max_lines_added. Include base_ref, files_changed, lines_added, max_files_changed, max_lines_added, and decomposition_skeleton."
    );
}

#[test]
fn activity_result_schema_reminds_pr_feedback_to_recheck_pr_state() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "address_pr_feedback"
        }),
    );
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "addressing_feedback",
        WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
        "local_review_gate"
    );
    assert_eq!(
        schema["activity_contract"]["accepted_artifacts"][1],
        PR_REPAIR_SNAPSHOT_ARTIFACT
    );
    assert_eq!(
        schema["activity_contract"]["accepted_artifacts"][2],
        ISSUE_STATE_ARTIFACT
    );
    assert_eq!(
        schema["activity_contract"]["accepted_signals"][0],
        ISSUE_CLOSED_SIGNAL
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "pr_repair_snapshot_with_action_and_passing_validation_or_closed_issue_evidence"
    );
    assert!(
        schema["transition_contract"]["on_succeeded"]["success_requires"]
            .as_str()
            .is_some_and(|value| value.contains("IssueClosed"))
    );
    assert!(schema["agent_summary_contract"]["must_include"]
        .as_array()
        .is_some_and(
            |items| items.contains(&json!("fresh PR state checked before final response"))
        ));
    assert!(schema["agent_summary_contract"]["must_include"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item
            .as_str()
            .is_some_and(|value| value.contains("pr_hygiene update/rebase")))));
    assert!(
        schema["transition_contract"]["on_succeeded"]["required_summary"]
            .as_str()
            .is_some_and(|value| value.contains("command_input.source=pr_hygiene"))
    );
    assert_eq!(
        schema["agent_summary_contract"]["artifacts"]["pr_repair_snapshot"]["required_unless"],
        "IssueClosed/IssueAlreadyResolved signal or issue_state artifact proves the issue or PR is already closed/resolved."
    );
    assert_eq!(
        schema["agent_summary_contract"]["artifacts"]["issue_state"]["required_when"],
        "No repair is needed because the issue or PR is already closed/resolved."
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["IssueClosed"],
        "Use when the issue or PR is confirmed closed and no feedback repair is needed. Include state=closed or state=resolved plus issue_number or issue_url."
    );
    let snapshot_fields = schema["agent_summary_contract"]["artifacts"]["pr_repair_snapshot"]
        ["fields"]
        .as_array()
        .expect("pr_repair_snapshot fields should be an array");
    assert!(snapshot_fields.contains(&json!("head_sha")));
    assert!(snapshot_fields.contains(&json!("head_oid")));
    assert!(snapshot_fields.contains(&json!("action_taken")));
    assert!(snapshot_fields.contains(&json!("no_code_change_reason")));
    assert!(!snapshot_fields.contains(&json!("head_sha_or_head_oid")));
    assert!(!snapshot_fields.contains(&json!("action_taken_or_no_code_change_reason")));
    assert!(
        schema["agent_summary_contract"]["artifacts"]["pr_repair_snapshot"]["field_contract"]
            ["validation_commands"]
            .as_str()
            .is_some_and(|value| value.contains("successful status"))
    );
}

#[test]
fn activity_result_schema_describes_issue_planning_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": ISSUE_PLAN_ACTIVITY
        }),
    );
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "planning",
        WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123");

    let schema = activity_result_schema(&job, Some(&workflow));

    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["reducer_next_state"],
        "implementing"
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_artifacts"][0],
        ISSUE_PLAN_ARTIFACT
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_signals"][0],
        ISSUE_PLAN_READY_SIGNAL
    );
    assert_eq!(
        schema["activity_contract"]["success_requires"],
        "issue_plan_artifact_or_ready_signal"
    );
    assert_eq!(
        schema["agent_summary_contract"]["artifacts"]["issue_plan"]["required"],
        true
    );
    assert!(schema["agent_summary_contract"]["must_not_include"]
        .as_array()
        .is_some_and(|items| items.contains(&json!("repository code changes"))));
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
        schema["transition_contract"]["on_succeeded"]["accepted_artifacts"][1],
        SERVER_PR_SNAPSHOT_ARTIFACT
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["accepted_artifacts"][2],
        PR_FEEDBACK_SNAPSHOT_ARTIFACT
    );
    assert_eq!(
        schema["transition_contract"]["on_succeeded"]["parent_propagation"],
        "The same activity result is propagated to the parent github_issue_pr workflow; the parent starts quality_gate before ready_to_merge."
    );
    assert!(
        schema["transition_contract"]["on_succeeded"]["success_requires"]
            .as_str()
            .is_some_and(|value| value.contains("server_pr_snapshot"))
    );
    assert!(
        schema["transition_contract"]["structured_decision"]["description"]
            .as_str()
            .is_some_and(|value| value.contains("same server_pr_snapshot evidence"))
    );
    assert_eq!(
        schema["activity_contract"]["child_outcome_contract"],
        "pr_feedback_outcome"
    );
    assert_eq!(
        schema["agent_summary_contract"]["signals"]["PrReadyToMerge"],
        "Use only with server_pr_snapshot proving APPROVED reviewDecision, isDraft=false, SUCCESS checks, CLEAN mergeStateStatus, complete reviewThreads, and zero active unresolved review threads for the final head."
    );
    assert_eq!(
        schema["agent_summary_contract"]["artifacts"]["server_pr_snapshot"]["required_when"],
        "Using PrReadyToMerge."
    );
    assert_eq!(schema["agent_summary_contract"]["server_owned"], true);
    assert_eq!(
        schema["activity_contract"]["accepted_artifacts"][1],
        SERVER_PR_SNAPSHOT_ARTIFACT
    );
    let snapshot_fields = schema["agent_summary_contract"]["artifacts"]["server_pr_snapshot"]
        ["fields"]
        .as_array()
        .expect("server_pr_snapshot fields should be an array");
    assert!(snapshot_fields.contains(&json!("snapshot_source")));
    assert!(snapshot_fields.contains(&json!("head_oid")));
    assert!(snapshot_fields.contains(&json!("review_threads_complete")));
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
