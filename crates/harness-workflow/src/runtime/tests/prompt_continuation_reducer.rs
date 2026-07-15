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
        ("prompt_continuation_prompt_ref_missing", "blocked"),
        ("prompt_continuation_scope_too_large", "blocked"),
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
            let event =
                runtime_completion_event(&instance, PROMPT_TASK_IMPLEMENT_ACTIVITY, result);

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
fn prompt_scope_too_large_overrides_active_external_state() {
    let policy = PromptContinuationPolicy {
        max_attempts: 4,
        attempt_delay_secs: 0,
        active_states: std::collections::BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 3,
    };
    let instance = prompt_task_instance("implementing").with_data(json!({
        "prompt_ref": "prompt-scope-ref",
        "continuation": PromptContinuationState::initial(&policy),
    }));
    let result = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "The prompt task exceeded the safe implementation scope.",
    )
    .with_validation(ValidationRecord::new("cargo test", "passed"))
    .with_signal(ActivitySignal::new(
        "external_state",
        json!({ "state": "In Progress", "subject": "TEAM-1607" }),
    ))
    .with_signal(ActivitySignal::new(
        SCOPE_TOO_LARGE_SIGNAL,
        json!({
            "base_ref": "origin/main",
            "files_changed": 31,
            "lines_added": 1501,
            "max_files_changed": 30,
            "max_lines_added": 1500,
            "decomposition_skeleton": [{
                "title": "Split continuation work",
                "summary": "Implement the remaining scope as a separate task."
            }]
        }),
    ));
    let event = runtime_completion_event(&instance, PROMPT_TASK_IMPLEMENT_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("completion should parse")
        .expect("scope guard should produce a blocked decision");

    assert_eq!(decision.decision, "prompt_continuation_scope_too_large");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .all(|command| command.command_type != WorkflowCommandType::EnqueueActivity));
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "scope_too_large"));
}
