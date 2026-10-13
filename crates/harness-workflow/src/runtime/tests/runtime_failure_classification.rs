// GH-1584 stop-metadata classification coverage: blocked/failed payload
// builders persist `stop_reason_code` and the derived `reason_class` into the
// command payload and `last_stop` (B-001, B-002, B-012), and human-wait
// decision sites set terminal codes explicitly.

use crate::runtime::reason_class::{classify_stop, StopReasonClass};
use crate::runtime::LOCAL_REVIEW_BLOCKED_SIGNAL;

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
    // Unclassified blocks require operator input, including when the activity
    // did not supply an error kind.
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
    assert_eq!(payload["stop_reason_code"], "maintainer_input_required");
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
fn readiness_gate_local_review_preserves_explicit_stop_signals() {
    for status in [ActivityStatus::Succeeded, ActivityStatus::Blocked] {
        for separate_stop_signal in [false, true] {
            for (code, error_kind, expected_class) in [
                ("rate_limited", ActivityErrorKind::ExternalDependency, "transient"),
                ("ci_backend_unavailable", ActivityErrorKind::ExternalDependency, "transient"),
                ("maintainer_input_required", ActivityErrorKind::ExternalDependency, "terminal"),
                ("unrecognized_stop", ActivityErrorKind::ExternalDependency, "terminal"),
                ("rate_limited", ActivityErrorKind::Fatal, "terminal"),
                ("ci_backend_unavailable", ActivityErrorKind::Configuration, "terminal"),
            ] {
                let instance = issue_instance("local_review_gate")
                    .with_server_data(json!({ "pr_number": 2159 }));
                let mut result = ActivityResult::succeeded(LOCAL_REVIEW_ACTIVITY, "Review blocked.")
                    .with_error_kind(error_kind)
                    .with_signal(ActivitySignal::new(
                        LOCAL_REVIEW_BLOCKED_SIGNAL,
                        if separate_stop_signal {
                            json!({})
                        } else {
                            json!({ "stop_reason_code": code })
                        },
                    ));
                if separate_stop_signal {
                    result = result.with_signal(ActivitySignal::new(
                        "runtime_stop",
                        json!({ "stop_reason_code": code }),
                    ));
                }
                result.status = status;
                let event = runtime_completion_event(&instance, LOCAL_REVIEW_ACTIVITY, result);
                let decision = reduce_runtime_job_completed(&instance, &event)
                    .expect("completion should parse")
                    .expect("review should block");
                assert_eq!(decision.next_state, "blocked");
                let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
                for stop in [payload, &payload["last_stop"]] {
                    assert_eq!(stop["stop_reason_code"], code, "{status:?}, separate={separate_stop_signal}");
                    assert_eq!(stop["reason_class"], expected_class, "{code}, {error_kind:?}");
                }
            }
        }
    }
}

#[test]
fn readiness_gate_missing_human_approval_is_not_transient() {
    // Owner-queue completion 30f08dc1-1dec-4841-8b7c-3e0209812493 was
    // automatically unblocked eight seconds later without approval/head change.
    for status in [ActivityStatus::Blocked, ActivityStatus::Succeeded] {
        let instance = issue_instance("local_review_gate").with_server_data(json!({
            "repo": "majiayu000/remem",
            "pr_number": 1115,
            "runtime_retry_policy": { "max_failed_activity_retries": 6 }
        }));
        let mut result = ActivityResult::succeeded(
            LOCAL_REVIEW_ACTIVITY,
            "Code review and CI passed; explicit human security approval is absent.",
        )
        .with_error_kind(ActivityErrorKind::ExternalDependency)
        .with_signal(ActivitySignal::new(
            LOCAL_REVIEW_BLOCKED_SIGNAL,
            json!({
                "reviewed_head_sha": "943349d526012d4efad78d922f1012c970e74a63",
                "code_findings": [],
                "github_evidence": { "reviews": 0, "is_draft": true },
                "blocking_findings": [{
                    "kind": "missing_human_review",
                    "location": "CONTRIBUTING.md:76-79"
                }],
                "next_workflow_action": "Record exact-head human approval, then operator unblock."
            }),
        ));
        result.status = status;
        let event = runtime_completion_event(&instance, LOCAL_REVIEW_ACTIVITY, result);
        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("completion should parse")
            .expect("missing approval should block");

        assert_eq!(decision.next_state, "blocked");
        assert_eq!(decision.commands.len(), 1);
        let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
        assert_eq!(payload["stop_reason_code"], "maintainer_input_required");
        assert_eq!(payload["reason_class"], "terminal");
        assert_eq!(payload["last_stop"]["reason_class"], "terminal");
        assert_eq!(payload["last_stop"]["error_kind"], "external_dependency");
        assert_eq!(payload["last_stop"]["event_id"], event.id);
        assert_eq!(
            classify_stop(
                payload["last_stop"]["stop_reason_code"].as_str(),
                Some(ActivityErrorKind::ExternalDependency),
            ),
            StopReasonClass::Terminal,
        );
    }
}

#[test]
fn readiness_gate_delivered_draft_with_unchanged_permission_is_not_transient() {
    // keepline #138 job b76526b3 delivered #145 before macOS denied native
    // notification acceptance; re-running implementation cannot grant permission.
    let instance = issue_instance("implementing").with_server_data(json!({
        "runtime_retry_policy": { "max_failed_activity_retries": 6 }
    }));
    let mut result = ActivityResult::succeeded(
        "implement_issue",
        "Draft delivered; native notification clicks await macOS permission.",
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency)
    .with_artifact(ActivityArtifact::new(
        "pull_request",
        json!({
            "pr_number": 145,
            "pr_url": "https://github.com/majiayu000/keepline/pull/145",
            "draft": true,
            "head_sha": "9d6dbe5f5bc4c4a8771b13dea1c52183ff665547"
        }),
    ));
    result.status = ActivityStatus::Blocked;
    let event = runtime_completion_event(&instance, "implement_issue", result);
    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("completion should parse")
        .expect("unchanged permission should block");

    assert_eq!(decision.next_state, "blocked");
    assert_eq!(decision.commands.len(), 1);
    let payload = &mark_stop_command(&decision, WorkflowCommandType::MarkBlocked).command;
    assert_eq!(payload["stop_reason_code"], "maintainer_input_required");
    assert_eq!(payload["reason_class"], "terminal");
    assert_eq!(payload["last_stop"]["stop_reason_code"], "maintainer_input_required");
}

#[test]
fn readiness_gate_upstream_capacity_failure_retains_bounded_retry() {
    let instance = prompt_task_instance("implementing").with_server_data(json!({
        "runtime_retry_policy": { "max_failed_activity_retries": 1, "retry_delay_secs": 30 }
    }));
    let result = ActivityResult::failed(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "Agent upstream failure.",
        "Selected model is at capacity.",
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency);
    let mut event = runtime_completion_event(&instance, PROMPT_TASK_IMPLEMENT_ACTIVITY, result);
    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("completion should parse")
        .expect("upstream capacity failure should retry");
    assert_eq!(decision.decision, "retry_failed_prompt_task");
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    assert_eq!(decision.commands[0].command["retry_delay_secs"], 30);

    event.event["command"]["command"]["retry_attempt"] = json!(1);
    let exhausted = reduce_runtime_job_completed(&instance, &event)
        .expect("completion should parse")
        .expect("retry limit should stop further attempts");
    assert_eq!(exhausted.next_state, "failed");
    assert_eq!(exhausted.commands[0].command_type, WorkflowCommandType::MarkFailed);
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
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_signal(ActivitySignal::new(
            "runtime_stop",
            json!({ "stop_reason_code": "rate_limited" }),
        ));
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
fn runtime_transcript_store_failure_honors_explicit_zero_retry_limits() {
    for retry_policy in [
        json!({ "max_failed_activity_retries": 0 }),
        json!({
            "max_failed_activity_retries": 3,
            "activity_retries": {
                "exact_replay": { "max_failed_activity_retries": 0 }
            }
        }),
    ] {
        let mut instance = issue_instance("implementing");
        instance.data["runtime_retry_policy"] = retry_policy;
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
        let event = runtime_completion_event(&instance, "exact_replay", result);

        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("event should parse")
            .expect("zero-retry transcript failure should terminate");
        assert_eq!(decision.next_state, "failed");
        assert_eq!(decision.decision, "fail_after_runtime_activity");
    }
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
