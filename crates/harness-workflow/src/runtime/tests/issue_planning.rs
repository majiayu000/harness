use super::*;

#[test]
fn issue_submission_decision_force_execute_starts_implementation() {
    let labels = Vec::new();
    let instance = issue_instance("discovered");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-force",
            repo: Some("owner/repo"),
            issue_number: 123,
            labels: &labels,
            force_execute: true,
            additional_prompt: Some("skip planning for this operator-requested run"),
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
        },
    );

    assert_eq!(
        output.action,
        IssueSubmissionWorkflowAction::RunImplementation
    );
    assert_eq!(output.decision.next_state, "implementing");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("implement_issue")
    );
    assert_eq!(
        output.decision.commands[0].dedupe_key,
        "issue-submit:owner/repo:issue:123:task:task-force:implement"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("force-execute issue submission should validate");
}

#[test]
fn issue_submission_decision_uses_remote_fact_hash_for_implementation_dedupe() {
    let labels = Vec::new();
    let instance = issue_instance("discovered");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-force",
            repo: Some("owner/repo"),
            issue_number: 123,
            labels: &labels,
            force_execute: true,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: Some("sha256:abc"),
        },
    );

    assert_eq!(
        output.decision.commands[0].dedupe_key,
        "implement_issue:sha256:abc"
    );
    assert_eq!(
        output.decision.commands[0].command["dispatch_gate"]["reason"],
        "uncovered_issue_ready_for_implementation"
    );
    assert_eq!(
        output.decision.commands[0].command["dispatch_gate"]["fact_hash"],
        "sha256:abc"
    );
}

#[test]
fn issue_plan_success_starts_implementation_with_plan_payload() {
    let instance = issue_instance("planning");
    let plan_payload = json!({
        "summary": "Patch the PR repair completion reducer before touching prompts.",
        "task_class": "runtime_or_data",
        "target_files": [
            "crates/harness-workflow/src/runtime/reducer/pr_feedback_completion.rs"
        ],
        "validation_plan": ["cargo test -p harness-workflow pr_repair_evidence"],
        "blockers": []
    });
    let result = ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Issue plan ready.")
        .with_artifact(ActivityArtifact::new(
            super::super::ISSUE_PLAN_ARTIFACT,
            plan_payload.clone(),
        ));
    let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("issue planning success should start implementation");

    assert_eq!(decision.decision, "start_implementation_after_issue_plan");
    assert_eq!(decision.next_state, "implementing");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].activity_name(),
        Some("implement_issue")
    );
    assert_eq!(decision.commands[0].command["issue_plan"], plan_payload);
    assert_eq!(
        decision.commands[0].command["issue_plan_summary"],
        "Patch the PR repair completion reducer before touching prompts."
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("issue plan completion decision should validate");
}

#[test]
fn issue_plan_ready_signal_can_start_implementation() {
    let instance = issue_instance("planning");
    let signal_payload = json!({
        "plan_summary": "Use the existing workflow reducer contract.",
        "task_class": "standard_code",
        "target_files": ["crates/harness-workflow/src/runtime/reducer.rs"],
        "validation_plan": ["cargo test -p harness-workflow issue_planning"],
        "blockers": []
    });
    let result = ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Issue plan ready.")
        .with_signal(ActivitySignal::new(
            super::super::ISSUE_PLAN_READY_SIGNAL,
            signal_payload.clone(),
        ));
    let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("issue planning signal should start implementation");

    assert_eq!(decision.next_state, "implementing");
    assert_eq!(decision.commands[0].command["issue_plan"], signal_payload);
    assert_eq!(
        decision.commands[0].command["issue_plan_summary"],
        "Use the existing workflow reducer contract."
    );
}

#[test]
fn issue_plan_empty_success_blocks_as_invalid_agent_output() {
    let instance = issue_instance("planning");
    let result = ActivityResult::succeeded(
        super::super::ISSUE_PLAN_ACTIVITY,
        "Planning finished without a structured plan.",
    );
    let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("empty issue plan success should block");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("plan_issue succeeded without"));
    assert!(decision
        .commands
        .iter()
        .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("empty plan block decision should validate");
}

#[test]
fn issue_plan_invalid_payload_blocks_as_missing_plan_evidence() {
    let invalid_results = vec![
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Null issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!(null),
            )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Empty issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!({}),
            )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "String issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!("plan ready"),
            )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Array issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!([]),
            )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Summary-only issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!({"summary": "done"}),
            )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Unknown-field issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!({"foo": "bar"}),
            )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Missing blockers issue plan.")
            .with_artifact(ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!({
                    "summary": "Patch the reducer.",
                    "task_class": "standard_code",
                    "target_files": ["crates/harness-workflow/src/runtime/reducer/plan_issue_completion.rs"],
                    "validation_plan": ["cargo test -p harness-workflow issue_planning"]
                }),
            )),
        ActivityResult::succeeded(
            super::super::ISSUE_PLAN_ACTIVITY,
            "Missing task_class issue plan.",
        )
        .with_artifact(ActivityArtifact::new(
            super::super::ISSUE_PLAN_ARTIFACT,
            json!({
                    "summary": "Patch the reducer.",
                    "target_files": ["crates/harness-workflow/src/runtime/reducer/plan_issue_completion.rs"],
                    "validation_plan": ["cargo test -p harness-workflow issue_planning"],
                    "blockers": []
            }),
        )),
        ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Null issue plan signal.")
            .with_signal(ActivitySignal::new(
                super::super::ISSUE_PLAN_READY_SIGNAL,
                json!(null),
            )),
    ];

    for result in invalid_results {
        let instance = issue_instance("planning");
        let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

        let decision = reduce_runtime_job_completed(&instance, &event)
            .expect("event should parse")
            .expect("invalid issue plan payload should block");

        assert_eq!(decision.decision, "block_invalid_agent_output");
        assert_eq!(decision.next_state, "blocked");
        assert!(decision.reason.contains("plan_issue succeeded without"));
    }
}

#[test]
fn runtime_completion_reducer_retries_issue_plan_failure_when_policy_allows() {
    let instance = issue_instance("planning").with_data(json!({
        "runtime_retry_policy": {
            "max_failed_activity_retries": 1
        }
    }));
    let result = ActivityResult::failed(
        super::super::ISSUE_PLAN_ACTIVITY,
        "Issue planning failed.",
        "codex stdin not available",
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency);
    let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("failed issue plan should produce a retry decision");

    assert_eq!(decision.decision, "retry_failed_runtime_activity");
    assert_eq!(decision.next_state, "planning");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(
        decision.commands[0].command["activity"],
        super::super::ISSUE_PLAN_ACTIVITY
    );
    assert_eq!(decision.commands[0].command["retry_attempt"], 1);
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("issue plan retry decision should validate");
}
