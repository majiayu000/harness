use super::*;
use harness_workflow::runtime::{
    RegisteredWorkflowDefinition, RepoMemoryKind, RepoMemoryOutcome, RepoMemoryRecord,
    RetrievedRepoMemoryRecord, RuntimeKind, TransitionAllowlist, TransitionRule,
    WorkflowDefinitionRegistry, WorkflowProgressMode, WorkflowRuntimeRecoveryAction,
    WorkflowRuntimeStore, WorkflowStateDefinition, WorkflowSubject, ISSUE_PLAN_ACTIVITY,
    ISSUE_PLAN_ARTIFACT, ISSUE_PLAN_READY_SIGNAL, PR_REPAIR_SNAPSHOT_ARTIFACT,
    SERVER_PR_SNAPSHOT_ARTIFACT,
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
        "pull_request_artifact_or_closed_issue_or_scope_too_large_signal; deferred submission mode requires candidate_branch instead of pull_request"
    );
    assert!(schema["activity_contract"]["accepted_artifacts"]
        .as_array()
        .is_some_and(|items| items.contains(&json!(CANDIDATE_BRANCH_ARTIFACT))));
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
fn workflow_decision_contract_uses_a_registered_non_builtin_definition() {
    let mut registry = WorkflowDefinitionRegistry::new();
    registry
        .register(RegisteredWorkflowDefinition::new(
            "custom_review",
            vec![
                WorkflowStateDefinition::active(
                    "custom_review",
                    "queued",
                    WorkflowProgressMode::ExternalWait,
                ),
                WorkflowStateDefinition::active(
                    "custom_review",
                    "reviewing",
                    WorkflowProgressMode::OperatorGate,
                ),
            ],
            TransitionAllowlist::new(vec![TransitionRule::new("queued", "reviewing", [])]),
        ))
        .expect("custom workflow definition should register");
    let workflow = WorkflowInstance::new(
        "custom_review",
        1,
        "queued",
        WorkflowSubject::new("review", "review:123"),
    )
    .with_id("custom-review-1");

    let contract = workflow_decision_contract_with_resolver(Some(&workflow), |definition_id| {
        registry.decision_validator_for_definition(definition_id)
    });

    assert_eq!(contract["available"], true);
    assert_eq!(contract["workflow_definition"], "custom_review");
    assert_eq!(contract["allowed_transitions"][0]["from_state"], "queued");
    assert_eq!(
        contract["allowed_transitions"][0]["next_state"],
        "reviewing"
    );
    assert_eq!(
        contract["allowed_transitions"][0]["allowed_commands"],
        json!([])
    );
}

#[test]
fn progress_wake_paths_are_callable_for_non_command_modes() {
    let config = harness_core::config::workflow::WorkflowConfig::default();
    assert!(config.pr_feedback.enabled);
    assert!(config.runtime_dispatch.enabled);
    assert!(config.runtime_worker.enabled);

    let _issue_dependency_observer =
        crate::workflow_runtime_submission::release_ready_issue_dependencies;
    let _prompt_dependency_observer =
        crate::workflow_runtime_submission::release_ready_prompt_dependencies;
    let _local_review_selector = crate::workflow_runtime_pr_feedback::request_local_review;
    let _feedback_selector = crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep;
    let _merge_approval = crate::workflow_runtime_pr_feedback::approve_runtime_merge_by_workflow_id;
    let _recovery_action = WorkflowRuntimeStore::recover_stopped_instance;
    let _parent_propagation = WorkflowRuntimeStore::commit_parent_runtime_completion;

    assert_eq!(WorkflowRuntimeRecoveryAction::Unblock.as_str(), "unblock");
    assert_eq!(WorkflowRuntimeRecoveryAction::Retry.as_str(), "retry");
    let child = WorkflowInstance::new(
        PR_FEEDBACK_DEFINITION_ID,
        1,
        "feedback_found",
        WorkflowSubject::new("pr", "pr:77"),
    )
    .with_parent("github-issue-parent");
    assert_eq!(
        child.parent_workflow_id.as_deref(),
        Some("github-issue-parent")
    );
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
        &[],
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

#[test]
fn runtime_prompt_packet_describes_deferred_candidate_submission_contract() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "command": {
                "submission_mode": "deferred",
                "candidate": {
                    "candidate_id": "wf-1:candidate-group:issue-1449:c1",
                    "candidate_index": 1,
                },
            },
        }),
    );
    let workflow_document = WorkflowDocument::default();
    let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

    let packet = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/issue-1449-c1"),
        Path::new("/repo"),
        &runtime_profile,
        &workflow_document,
        &[],
    );

    assert_eq!(packet["runtime_contract"]["submission_mode"], "deferred");
    assert!(packet["runtime_contract"]["deferred_submission_contract"]
        .as_str()
        .is_some_and(|value| value.contains("Do not open")));
    assert!(
        packet["required_structured_output"]["candidate_branch_artifact"]
            .as_str()
            .is_some_and(|value| value.contains(CANDIDATE_BRANCH_ARTIFACT))
    );
}

#[test]
fn memory_inject_prompt_packet_includes_fenced_repo_memory_section() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "repo": "owner/repo"
        }),
    );
    let workflow_document = WorkflowDocument::default();
    let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    let record = RepoMemoryRecord::new(
        "owner/repo",
        "implement_issue",
        RepoMemoryOutcome::Failed,
        RepoMemoryKind::FailureLesson,
        json!({
            "lesson": "cargo clippy catches CI-only warnings",
            "validation": [{"command": "cargo clippy --workspace --all-targets -- -D warnings", "status": "failed"}]
        }),
    )
    .with_evidence_ref("workflow:run-1:event:event-1");
    let record_id = record.id.to_string();
    let repo_memory = vec![RetrievedRepoMemoryRecord {
        record,
        estimated_tokens: 64,
    }];

    let packet = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &runtime_profile,
        &workflow_document,
        &repo_memory,
    );

    assert_eq!(
        packet["repo_memory"]["schema"],
        "harness.runtime.repo_memory.v1"
    );
    assert_eq!(
        packet["repo_memory"]["preamble"],
        REPO_MEMORY_PROMPT_PREAMBLE
    );
    assert_eq!(packet["repo_memory"]["records"][0]["id"], record_id);
    assert_eq!(packet["repo_memory"]["records"][0]["outcome"], "failed");
    assert_eq!(
        packet["repo_memory"]["records"][0]["kind"],
        "failure_lesson"
    );
    assert_eq!(
        packet["repo_memory"]["records"][0]["payload"]["lesson"],
        "cargo clippy catches CI-only warnings"
    );

    let prompt = build_runtime_job_prompt(&packet, None);
    assert!(prompt.contains("Repo memory:"));
    assert!(prompt.contains("```repo-memory"));
    assert!(prompt.contains(REPO_MEMORY_PROMPT_PREAMBLE));
    assert!(prompt.contains("\"lesson\": \"cargo clippy catches CI-only warnings\""));
}

#[test]
fn memory_inject_fresh_repo_gets_no_repo_memory_section() {
    let job = RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "repo": "owner/fresh"
        }),
    );
    let workflow_document = WorkflowDocument::default();
    let runtime_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

    let packet = build_runtime_prompt_packet(
        &job,
        None,
        Path::new("/workspaces/job-1"),
        Path::new("/repo"),
        &runtime_profile,
        &workflow_document,
        &[],
    );

    assert!(packet.get("repo_memory").is_none());
    let prompt = build_runtime_job_prompt(&packet, None);
    assert!(!prompt.contains("Repo memory:"));
    assert!(!prompt.contains("```repo-memory"));
}
