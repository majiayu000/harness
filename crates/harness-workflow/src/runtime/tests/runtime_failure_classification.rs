// GH-1584 stop-metadata classification coverage: blocked/failed payload
// builders persist `stop_reason_code` and the derived `reason_class` into the
// command payload and `last_stop` (B-001, B-002, B-012), and human-wait
// decision sites set terminal codes explicitly.

fn mark_stop_command(decision: &WorkflowDecision, command_type: WorkflowCommandType) -> &WorkflowCommand {
    decision
        .commands
        .iter()
        .find(|command| command.command_type == command_type)
        .expect("decision should carry the stop command")
}

#[test]
fn runtime_failure_blocked_payload_persists_signal_stop_reason_code_and_class() {
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "GitHub API rate limited".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "runtime_stop",
            json!({ "stop_reason_code": "rate_limited" }),
        )],
        validation: Vec::new(),
        error: Some("GitHub API rate limited".to_string()),
        error_kind: None,
    };
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("blocked result should produce a decision");
    assert_eq!(decision.next_state, "blocked");
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
    assert_eq!(payload["stop_reason_code"], json!("rate_limited"));
    assert_eq!(payload["reason_class"], json!("transient"));
    assert_eq!(payload["last_stop"]["stop_reason_code"], json!("rate_limited"));
    assert_eq!(payload["last_stop"]["reason_class"], json!("transient"));
    assert_eq!(payload["last_stop"]["event_id"], json!(event.id));
}

#[test]
fn runtime_failure_blocked_payload_without_code_classifies_terminal() {
    // B-002: no structured code and no error_kind fails closed, and the
    // decision is visible in the persisted payload.
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "needs a human".to_string(),
        artifacts: Vec::new(),
        signals: Vec::new(),
        validation: Vec::new(),
        error: Some("needs a human".to_string()),
        error_kind: None,
    };
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("blocked result should produce a decision");
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
    assert!(payload.get("stop_reason_code").is_none());
    assert_eq!(payload["reason_class"], json!("terminal"));
    assert_eq!(payload["last_stop"]["reason_class"], json!("terminal"));
}

#[test]
fn runtime_failure_failed_payload_classifies_from_error_kind() {
    let instance = issue_instance("implementing");
    for (error_kind, expected_class) in [
        (ActivityErrorKind::Timeout, "transient"),
        (ActivityErrorKind::ExternalDependency, "transient"),
        (ActivityErrorKind::Fatal, "terminal"),
        (ActivityErrorKind::Unknown, "terminal"),
    ] {
        let result = ActivityResult::failed("implement_issue", "failed", "agent turn failed")
            .with_error_kind(error_kind);
        let event = runtime_completion_event(&instance, "implement_issue", result);
        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("event should parse")
            .expect("failed result should produce a decision");
        assert_eq!(decision.next_state, "failed");
        let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkFailed).command;
        assert_eq!(
            payload["reason_class"],
            json!(expected_class),
            "error_kind {error_kind:?}"
        );
        assert_eq!(payload["last_stop"]["reason_class"], json!(expected_class));
    }
}

#[test]
fn runtime_failure_failed_payload_forged_transient_code_with_fatal_is_terminal() {
    // B-012: a forged transient signal code never overrides a fatal failure.
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Failed,
        summary: "fatal".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "runtime_stop",
            json!({ "stop_reason_code": "rate_limited" }),
        )],
        validation: Vec::new(),
        error: Some("fatal".to_string()),
        error_kind: Some(ActivityErrorKind::Fatal),
    };
    let event = runtime_completion_event(&instance, "implement_issue", result);
    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("failed result should produce a decision");
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkFailed).command;
    assert_eq!(payload["stop_reason_code"], json!("rate_limited"));
    assert_eq!(payload["reason_class"], json!("terminal"));
}

#[test]
fn runtime_failure_missing_implementation_block_sets_invalid_agent_output_code() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.");
    let event = runtime_completion_event(&instance, "implement_issue", result);
    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("implementation success without PR evidence should block");
    assert_eq!(decision.decision, "block_missing_implementation_result");
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
    assert_eq!(payload["stop_reason_code"], json!("invalid_agent_output"));
    assert_eq!(payload["reason_class"], json!("terminal"));
}

#[test]
fn runtime_failure_scope_too_large_block_sets_maintainer_input_required_code() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Scope guard rejected.").with_signal(
        ActivitySignal::new(
            SCOPE_TOO_LARGE_SIGNAL,
            json!({
                "base_ref": "origin/main",
                "files_changed": 31,
                "lines_added": 200,
                "max_files_changed": 30,
                "max_lines_added": 1500,
                "decomposition_skeleton": [{ "title": "split" }]
            }),
        ),
    );
    let event = runtime_completion_event(&instance, "implement_issue", result);
    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("scope guard signal should block");
    assert_eq!(decision.decision, "block_scope_too_large");
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
    assert_eq!(
        payload["stop_reason_code"],
        json!("maintainer_input_required")
    );
    assert_eq!(payload["reason_class"], json!("terminal"));
}

#[test]
fn runtime_transcript_store_failure_uses_bounded_default_backoff() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::failed(
        "exact_replay",
        "transcript preflight failed",
        "transcript store unavailable",
    )
    .with_error_kind(ActivityErrorKind::Retryable)
    .with_signal(ActivitySignal::new(
        "RuntimeTranscriptUnavailable",
        json!({
            "stop_reason_code": "runtime_transcript_store_unavailable",
        }),
    ));
    let event = runtime_completion_event(&instance, "exact_replay", result.clone());

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("transient transcript failure should retry");
    assert_eq!(decision.next_state, "implementing");
    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    assert_eq!(decision.commands[0].command["max_failed_activity_retries"], 3);
    assert_eq!(decision.commands[0].command["retry_delay_secs"], 5);

    let mut exhausted = runtime_completion_event(&instance, "exact_replay", result);
    exhausted.event["command"]["command"]["retry_attempt"] = json!(3);
    let decision = reduce_runtime_job_completed(&instance, &exhausted)
        .expect("event should parse")
        .expect("exhausted transcript failure should terminate");
    assert_eq!(decision.next_state, "failed");
    assert_eq!(decision.decision, "fail_after_runtime_activity");
}

#[test]
fn confirmed_runtime_transcript_loss_is_terminal() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::failed(
        "implement_issue",
        "transcript preflight failed",
        "required transcript is missing; reconstruct it before retrying",
    )
    .with_error_kind(ActivityErrorKind::Fatal)
    .with_signal(ActivitySignal::new(
        "RuntimeTranscriptUnavailable",
        json!({ "stop_reason_code": "runtime_transcript_lost" }),
    ));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("confirmed transcript loss should terminate");
    assert_eq!(decision.next_state, "failed");
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkFailed).command;
    assert_eq!(payload["stop_reason_code"], "runtime_transcript_lost");
    assert_eq!(payload["reason_class"], "terminal");
    assert!(payload["retry_hint"]
        .as_str()
        .expect("retry hint")
        .contains("non-retryable"));
}
