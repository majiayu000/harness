#[test]
fn runtime_completion_reducer_blocks_scope_too_large_signal() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Scope guard rejected the implementation before PR creation.",
    )
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 31,
            "lines_added": 200,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [
                {
                    "title": "Split workflow contract changes",
                    "summary": "Handle prompt/schema changes separately from reducer behavior.",
                    "target_files": ["crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs"]
                }
            ]
        }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("scope guard signal should block for decomposition");

    assert_eq!(decision.decision, "block_scope_too_large");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    let operator_command = decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention)
        .expect("operator attention should carry decomposition evidence");
    assert_eq!(operator_command.command["scope_guard"]["files_changed"], 31);
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "scope_too_large"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("scope-too-large block decision should validate");
}

#[test]
fn runtime_completion_reducer_blocks_scope_too_large_signal_from_blocked_result() {
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Scope guard rejected the implementation before PR creation.".to_string(),
        artifacts: Vec::new(),
        signals: Vec::new(),
        validation: Vec::new(),
        error: None,
        error_kind: None,
    }
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 42,
            "lines_added": 1600,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [
                {
                    "title": "Split reducer behavior",
                    "summary": "Keep workflow reducer changes separate from worker prompt updates.",
                    "target_files": ["crates/harness-workflow/src/runtime/reducer.rs"]
                }
            ]
        }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("blocked scope guard signal should block with decomposition evidence");

    assert_eq!(decision.decision, "block_scope_too_large");
    assert_eq!(decision.next_state, "blocked");
    let operator_command = decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention)
        .expect("operator attention should carry decomposition evidence");
    assert_eq!(operator_command.command["scope_guard"]["files_changed"], 42);
    assert_eq!(
        operator_command.command["scope_guard"]["decomposition_skeleton"][0]["title"],
        "Split reducer behavior"
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "scope_too_large"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocked scope-too-large decision should validate");
}

#[test]
fn runtime_completion_reducer_rejects_scope_too_large_signal_without_exceeded_threshold() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Scope guard payload did not exceed configured thresholds.",
    )
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 30,
            "lines_added": 1500,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [
                {
                    "title": "No split needed",
                    "summary": "The diff is within configured guard thresholds."
                }
            ]
        }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("non-exceeded scope guard signal should not satisfy implement_issue");

    assert_eq!(decision.decision, "block_missing_implementation_result");
    assert_eq!(decision.next_state, "blocked");
}
