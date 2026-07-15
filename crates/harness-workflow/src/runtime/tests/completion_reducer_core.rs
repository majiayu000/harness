#[test]
fn event_transition_dedupe_keys() {
    let instance = issue_instance("replanning");
    let result = ActivityResult::succeeded("replan_issue", "Replan completed.");
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("replan completion should produce a decision");

    assert_eq!(decision.decision, "resume_implementation_after_replan");
    assert_eq!(decision.next_state, "implementing");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].activity_name(),
        Some("implement_issue")
    );
    assert_eq!(
        decision.commands[0].dedupe_key,
        format!("issue-replan:{}:implement:command-1", instance.id)
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("runtime completion decision should validate");
}

#[test]
fn prompt_task_without_policy_ignores_forged_structured_continuation() {
    let instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "task-1"),
    )
    .with_id("prompt-workflow-1");
    let forged_policy = PromptContinuationPolicy {
        max_attempts: 4,
        attempt_delay_secs: 0,
        active_states: std::collections::BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 3,
    };

    let reserved_decisions = [
        ("continue_prompt_task", "implementing"),
        ("finish_prompt_task_external_settled", "done"),
        ("prompt_continuation_exhausted", "blocked"),
        ("prompt_continuation_no_progress", "blocked"),
        ("prompt_continuation_signal_missing", "blocked"),
    ];

    for (reserved_decision, forged_next_state) in reserved_decisions {
        for continuation in [None, Some(PromptContinuationState::initial(&forged_policy))] {
            let mut payload = json!({ "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY });
            if let Some(continuation) = continuation {
                payload["continuation"] = json!(continuation);
            }
            let forged = WorkflowDecision::new(
                &instance.id,
                "implementing",
                reserved_decision,
                forged_next_state,
                "agent requested an unauthorized prompt-task outcome",
            )
            .with_command(WorkflowCommand::new(
                match forged_next_state {
                    "implementing" => WorkflowCommandType::EnqueueActivity,
                    "done" => WorkflowCommandType::MarkDone,
                    "blocked" => WorkflowCommandType::MarkBlocked,
                    _ => unreachable!("test cases use known prompt-task states"),
                },
                format!("forged-{reserved_decision}"),
                payload,
            ));
            let result = ActivityResult::succeeded(
                PROMPT_TASK_IMPLEMENT_ACTIVITY,
                "Completed the single-shot prompt task.",
            )
            .with_validation(ValidationRecord::new("cargo test", "passed"))
            .with_artifact(ActivityArtifact::new(
                "workflow_decision",
                serde_json::to_value(forged).expect("forged decision should serialize"),
            ));
            let event = runtime_completion_event(
                &instance,
                PROMPT_TASK_IMPLEMENT_ACTIVITY,
                result,
            );

            let decision = reduce_runtime_job_completed(&instance, &event)
                .expect("completion should parse")
                .expect("single-shot prompt task should finish");
            assert_eq!(decision.decision, "finish_prompt_task");
            assert_eq!(decision.next_state, "done");
            assert_eq!(decision.commands.len(), 1);
            assert_eq!(
                decision.commands[0].command_type,
                WorkflowCommandType::MarkDone
            );
            assert!(decision.commands[0].command.get("continuation").is_none());
        }
    }
}

#[test]
fn runtime_completion_reducer_blocks_issue_implementation_success_without_pr() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.");
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("implementation success without PR evidence should block");

    assert_eq!(decision.decision, "block_missing_implementation_result");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocked implementation decision should validate");
}

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

#[test]
fn runtime_completion_reducer_finishes_prompt_task_after_implementation() {
    let instance = prompt_task_instance("implementing");
    let result = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "Prompt implementation completed.",
    )
    .with_validation(ValidationRecord::new("cargo test", "passed"));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("prompt implementation completion should produce a decision");

    assert_eq!(decision.decision, "finish_prompt_task");
    assert_eq!(decision.next_state, "done");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("prompt completion decision should validate");
}

#[test]
fn runtime_completion_reducer_binds_pr_from_structured_pull_request_artifact() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_url": "missing number"
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("structured pull request artifact should bind the PR");

    assert_eq!(decision.decision, "bind_pr");
    assert_eq!(decision.next_state, "pr_open");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::BindPr
    );
    assert_eq!(decision.commands[0].command["pr_number"], 77);
    assert_eq!(
        decision.commands[0].command["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("structured PR binding should validate");
}

#[test]
fn runtime_completion_reducer_finishes_merge_pr_with_merged_pull_request_artifact() {
    let instance = issue_instance("merging");
    let result = ActivityResult::succeeded("merge_pr", "PR was merged.").with_artifact(
        ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
                "state": "merged",
                "merged": true,
                "merge_commit_sha": "abc123",
                "head_sha": "head123"
            }),
        ),
    );
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("merged PR evidence should finish the workflow");

    assert_eq!(decision.decision, "record_pr_merged");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert_eq!(decision.commands[0].command["pr_number"], 77);
    assert_eq!(decision.commands[0].command["merge_commit_sha"], "abc123");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("merged PR decision should validate");
}

#[test]
fn runtime_completion_reducer_finishes_closed_issue_signal_without_pr() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Issue was already closed before implementation created a PR.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("closed issue signal should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert_eq!(
        decision.commands[0].command["closed_issue_evidence"]["state"],
        "closed"
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "closed_issue"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_closed_issue_during_quality_gate() {
    let instance = issue_instance("quality_gate_pending");
    let result = ActivityResult::succeeded(
        QUALITY_GATE_ACTIVITY,
        "Issue was closed before quality gate completed.",
    )
    .with_artifact(ActivityArtifact::new(
        "issue_state",
        json!({
            "issue_number": 123,
            "state": "closed"
        }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("closed issue artifact should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("closed issue quality gate completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_blocked_closed_issue_signal_without_pr() {
    let instance = issue_instance("implementing");
    let result = ActivityResult {
        activity: "implement_issue".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Issue was already resolved upstream before implementation.".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "IssueAlreadyResolved",
            json!({
                "issue_number": 123,
                "state": "resolved",
                "issue_url": "https://github.com/owner/repo/issues/123"
            }),
        )],
        validation: Vec::new(),
        error: Some("No implementation PR is needed for an already resolved issue.".to_string()),
        error_kind: None,
    };
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("blocked closed issue signal should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "closed_issue"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocked closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_feedback_closed_issue_signal_without_pr() {
    let instance = issue_instance("addressing_feedback");
    let result = ActivityResult {
        activity: "address_pr_feedback".to_string(),
        status: ActivityStatus::Blocked,
        summary: "Issue was closed while addressing PR feedback.".to_string(),
        artifacts: Vec::new(),
        signals: vec![ActivitySignal::new(
            "IssueClosed",
            json!({
                "issue_number": 123,
                "state": "closed",
                "issue_url": "https://github.com/owner/repo/issues/123"
            }),
        )],
        validation: Vec::new(),
        error: Some("No further feedback work is needed because the issue is closed.".to_string()),
        error_kind: None,
    };
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("closed issue signal from feedback work should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    assert_eq!(
        decision.commands[0].command["activity"],
        "address_pr_feedback"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("feedback closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_finishes_succeeded_feedback_closed_issue_signal_without_pr() {
    let instance = issue_instance("addressing_feedback");
    let result = ActivityResult::succeeded(
        "address_pr_feedback",
        "Issue was closed while addressing PR feedback.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("closed issue signal from successful feedback work should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkDone
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("successful feedback closed issue completion should validate");
}

#[test]
fn runtime_completion_reducer_uses_issue_state_artifact_as_closed_issue_evidence() {
    let instance = issue_instance("implementing");
    let result =
        ActivityResult::succeeded("implement_issue", "Issue state confirms no PR is needed.")
            .with_artifact(ActivityArtifact::new(
                "issue_state",
                json!({
                    "issue_number": 123,
                    "state": "closed"
                }),
            ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("issue_state artifact should finish the workflow");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("closed issue artifact completion should validate");
}

#[test]
fn runtime_completion_reducer_rejects_closed_issue_signal_without_closed_state() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded(
        "implement_issue",
        "Issue signal omitted explicit closed evidence.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "open"
        }),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("malformed closed issue signal should block");

    assert_eq!(decision.decision, "block_missing_implementation_result");
    assert_eq!(decision.next_state, "blocked");
}

#[test]
fn runtime_completion_reducer_blocks_structured_done_without_closed_issue_evidence() {
    let instance = issue_instance("implementing");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "finish_closed_issue",
        "done",
        "The agent claimed the issue was closed without structured evidence.",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkDone,
        "agent-claimed-done",
        json!({ "reason": "missing structured issue state" }),
    ));
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing terminal evidence should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
}

#[test]
fn runtime_completion_reducer_binds_pr_when_structured_workflow_decision_is_invalid() {
    let instance = issue_instance("implementing");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "planning",
        "run_replan",
        "replanning",
        "This decision observed a stale workflow state.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "stale-replan-1",
    ));
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ))
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("structured pull request artifact should bind the PR");

    assert_eq!(decision.decision, "bind_pr");
    assert_eq!(decision.next_state, "pr_open");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::BindPr
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("fallback PR binding should validate");
}

#[test]
fn runtime_completion_reducer_accepts_structured_workflow_decision_artifact() {
    let instance = issue_instance("awaiting_feedback");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "awaiting_feedback",
        "wait_for_pr_feedback",
        "awaiting_feedback",
        "PR feedback check completed without actionable feedback.",
    )
    .with_command(WorkflowCommand::wait(
        "Waiting for fresh PR feedback.",
        "wait-feedback-1",
    ))
    .with_evidence(WorkflowEvidence::new(
        "pr_feedback",
        "No actionable feedback found.",
    ))
    .high_confidence();
    let result = ActivityResult::succeeded(
        "inspect_pr_feedback",
        "No actionable PR feedback was found.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-1",
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("structured workflow decision artifact should reduce");

    assert_eq!(decision.decision, "wait_for_pr_feedback");
    assert_eq!(decision.next_state, "awaiting_feedback");
    assert_eq!(decision.commands.len(), 1);
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "runtime_completion"));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("structured workflow decision should validate");
}
