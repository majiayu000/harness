use super::*;
use crate::runtime::WorkflowDefinitionRegistry;

fn attested_classifier_result(
    verdict: &str,
    summary: &str,
    runtime_job_id: &str,
) -> ActivityResult {
    let assessment = json!({
        "schema": "harness.runtime.classifier_assessment.v1",
        "verdict": verdict,
        "rationale": summary,
        "evidence_refs": [],
        "subject_head_oid": null,
        "attestation": {
            "runtime_job_id": runtime_job_id,
            "runtime_profile": "classifier-default",
            "requested_model": "gpt-test",
            "model": "gpt-test",
            "reported_models": ["gpt-test"],
            "prompt_packet_digest": "sha256:prompt",
            "policy_sha256": "policy-digest",
        }
    });
    ActivityResult::succeeded(super::super::CHANGE_SCOPE_REVIEW_ACTIVITY, summary)
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_CLASSIFIER_ASSESSMENT,
            assessment.clone(),
        ))
        .with_signal(ActivitySignal::new(verdict, assessment))
}

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
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
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
        format!("{}:discovered:submit", instance.id)
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
fn issue_submission_decision_keeps_remote_fact_hash_out_of_initial_dedupe() {
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
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );

    assert_eq!(
        output.decision.commands[0].dedupe_key,
        format!("{}:discovered:submit", instance.id)
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
fn submission_mode_threads_through_issue_submission_commands() {
    let labels = Vec::new();
    let instance = issue_instance("discovered");

    for (mode, expected) in [
        (SubmissionMode::Immediate, "immediate"),
        (SubmissionMode::Deferred, "deferred"),
    ] {
        let output = build_issue_submission_decision(
            &instance,
            IssueSubmissionDecisionInput {
                task_id: "task-submission-mode",
                repo: Some("owner/repo"),
                issue_number: 123,
                labels: &labels,
                force_execute: true,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                remote_fact_hash: None,
                submission_mode: mode,
                candidate_fanout: None,
            },
        );

        assert_eq!(output.decision.next_state, "implementing");
        assert_eq!(
            output.decision.commands[0].activity_name(),
            Some("implement_issue")
        );
        assert_eq!(
            output.decision.commands[0].command["submission_mode"],
            expected
        );
    }
}

#[test]
fn candidate_fanout_force_execute_starts_deferred_candidate_commands() -> anyhow::Result<()> {
    let labels = vec!["best-of-n".to_string()];
    let instance = issue_instance("discovered");
    let fanout = CandidateFanoutRequest {
        candidate_group_id: "wf-1:candidate-group:issue-123".to_string(),
        candidate_count: 2,
        trigger_label: "best-of-n".to_string(),
        max_turns_per_candidate: Some(4),
    };

    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-candidates",
            repo: Some("owner/repo"),
            issue_number: 123,
            labels: &labels,
            force_execute: true,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: Some("sha256:fanout"),
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: Some(fanout),
        },
    );

    assert_eq!(
        output.action,
        IssueSubmissionWorkflowAction::RunImplementation
    );
    assert_eq!(output.decision.commands.len(), 2);
    assert_eq!(
        output.decision.commands[0].dedupe_key,
        format!("{}:discovered:submit:candidate:c1", instance.id)
    );
    assert_eq!(
        output.decision.commands[1].dedupe_key,
        format!("{}:discovered:submit:candidate:c2", instance.id)
    );
    for (index, command) in output.decision.commands.iter().enumerate() {
        let candidate_index = index + 1;
        assert_eq!(command.activity_name(), Some("implement_issue"));
        assert_eq!(command.command["submission_mode"], "deferred");
        assert_eq!(
            command.command["candidate"]["candidate_index"],
            candidate_index
        );
        assert_eq!(command.command["candidate"]["candidate_count"], 2);
        assert_eq!(
            command.command["candidate"]["budget"]["max_turns_per_candidate"],
            4
        );
    }
    DecisionValidator::github_issue_pr().validate(
        &instance,
        &output.decision,
        &ValidationContext::new("workflow-policy", Utc::now()),
    )?;
    Ok(())
}

#[test]
fn issue_plan_success_starts_scope_review_with_plan_payload() {
    let instance = current_issue_instance("planning");
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
        .expect("issue planning success should start scope review");

    assert_eq!(decision.decision, "review_issue_plan_scope");
    assert_eq!(decision.next_state, "plan_scope_review");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].activity_name(),
        Some(super::super::CHANGE_SCOPE_REVIEW_ACTIVITY)
    );
    assert_eq!(
        decision.commands[0].command["scope_facts"]["issue_plan"],
        plan_payload
    );
    assert_eq!(
        decision.commands[0].command["scope_facts"]["issue_plan_summary"],
        "Patch the PR repair completion reducer before touching prompts."
    );
    assert_eq!(
        decision.commands[0].command["classifier_continuations"]["implementing"]["submission_mode"],
        "immediate"
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
fn legacy_issue_plan_completions_keep_the_v1_direct_implementation_path() -> anyhow::Result<()> {
    for (state, activity) in [
        ("planning", super::super::ISSUE_PLAN_ACTIVITY),
        ("replanning", "replan_issue"),
    ] {
        let instance = legacy_issue_instance(state);
        let result = ActivityResult::succeeded(activity, "Issue plan ready.").with_artifact(
            ActivityArtifact::new(
                super::super::ISSUE_PLAN_ARTIFACT,
                json!({
                    "summary": "Keep the historical workflow moving.",
                    "task_class": "standard_code",
                    "target_files": ["src/lib.rs"],
                    "validation_plan": ["cargo test -p harness-workflow issue_planning"],
                    "blockers": []
                }),
            ),
        );
        let event = runtime_completion_event(&instance, activity, result);
        let decision = reduce_runtime_job_completed(&instance, &event)?
            .ok_or_else(|| anyhow::anyhow!("legacy plan completion should reduce"))?;

        assert_eq!(decision.next_state, "implementing");
        assert_eq!(decision.commands.len(), 1);
        assert_eq!(
            decision.commands[0].activity_name(),
            Some("implement_issue")
        );
        WorkflowDefinitionRegistry::with_builtins()
            .decision_validator_for_instance(&instance)
            .expect("legacy pin should resolve")
            .expect("legacy definition should have a validator")
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime-1", Utc::now()),
            )?;
    }
    Ok(())
}

#[test]
fn candidate_fanout_issue_plan_completion_uses_persisted_metadata() -> anyhow::Result<()> {
    let fanout = CandidateFanoutRequest {
        candidate_group_id: "wf-1:candidate-group:issue-123".to_string(),
        candidate_count: 2,
        trigger_label: "best-of-n".to_string(),
        max_turns_per_candidate: None,
    };
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "planning",
        WorkflowSubject::new("issue", "123"),
    )
    .with_server_data(json!({
        "definition_hash": github_issue_pr_definition_hash(),
        "candidate_fanout": fanout.clone(),
    }));
    let plan_payload = json!({
        "summary": "Patch the submission reducer.",
        "task_class": "runtime_or_data",
        "target_files": [
            "crates/harness-workflow/src/runtime/submission.rs"
        ],
        "validation_plan": ["cargo test -p harness-workflow candidate_fanout"],
        "blockers": []
    });
    let result = ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Issue plan ready.")
        .with_artifact(ActivityArtifact::new(
            super::super::ISSUE_PLAN_ARTIFACT,
            plan_payload,
        ));
    let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)?
        .ok_or_else(|| anyhow::anyhow!("issue planning success should start scope review"))?;

    assert_eq!(decision.decision, "review_issue_plan_scope");
    assert_eq!(decision.next_state, "plan_scope_review");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(
        decision.commands[0].command["classifier_continuations"]["implementing"]
            ["apply_candidate_fanout"],
        true
    );
    DecisionValidator::github_issue_pr().validate(
        &instance,
        &decision,
        &ValidationContext::new("runtime-1", Utc::now()),
    )?;

    let scope_instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "plan_scope_review",
        WorkflowSubject::new("issue", "123"),
    )
    .with_server_data(json!({
        "definition_hash": github_issue_pr_definition_hash(),
        "candidate_fanout": fanout,
    }));
    let classifier_result =
        attested_classifier_result("allow", "Scope is coherent.", "classifier-job");
    let classifier_event = WorkflowEvent::new(
        &scope_instance.id,
        2,
        super::super::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "classifier-command",
        "command": decision.commands[0].clone(),
        "runtime_job_id": "classifier-job",
        "activity_result": classifier_result,
    }));
    let implementation = reduce_runtime_job_completed(&scope_instance, &classifier_event)?
        .ok_or_else(|| anyhow::anyhow!("allow verdict should start implementation"))?;

    assert_eq!(implementation.next_state, "implementing");
    assert_eq!(implementation.commands.len(), 2);
    assert!(implementation
        .commands
        .iter()
        .all(|command| command.activity_name() == Some("implement_issue")));
    assert_eq!(
        implementation.commands[0].command["candidate"]["candidate_index"],
        1
    );
    assert_eq!(
        implementation.commands[1].command["candidate"]["candidate_index"],
        2
    );
    DecisionValidator::github_issue_pr().validate(
        &scope_instance,
        &implementation,
        &ValidationContext::new("runtime-1", Utc::now()),
    )?;
    Ok(())
}

#[test]
fn legacy_replan_completion_is_requeued_under_structured_contract() -> anyhow::Result<()> {
    let instance = current_issue_instance("replanning");
    let result =
        ActivityResult::succeeded("replan_issue", "Legacy replan completed.").with_artifact(
            ActivityArtifact::new("workflow_decision", json!({"decision": "continue"})),
        );
    let event = runtime_completion_event(&instance, "replan_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)?
        .ok_or_else(|| anyhow::anyhow!("legacy replan should be migrated"))?;

    assert_eq!(decision.decision, "retry_replan_with_structured_contract");
    assert_eq!(decision.next_state, "replanning");
    assert_eq!(decision.commands[0].activity_name(), Some("replan_issue"));
    assert_eq!(
        decision.commands[0].command["structured_issue_plan_contract"],
        true
    );
    DecisionValidator::github_issue_pr().validate(
        &instance,
        &decision,
        &ValidationContext::new("runtime-1", Utc::now()),
    )?;
    Ok(())
}

#[test]
fn structured_replan_contract_failure_does_not_retry_forever() -> anyhow::Result<()> {
    let instance = current_issue_instance("replanning");
    let result =
        ActivityResult::succeeded("replan_issue", "Invalid new-contract output.").with_artifact(
            ActivityArtifact::new("workflow_decision", json!({"decision": "continue"})),
        );
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::reducer::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "command-2",
        "command": WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "structured-replan",
            json!({
                "activity": "replan_issue",
                "structured_issue_plan_contract": true,
            }),
        ),
        "runtime_job_id": "job-2",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)?
        .ok_or_else(|| anyhow::anyhow!("invalid structured replan should block"))?;

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    Ok(())
}

#[test]
fn submission_mode_deferred_survives_issue_plan_completion() {
    let instance = current_issue_instance("planning");
    let plan_payload = json!({
        "summary": "Patch the submission reducer.",
        "task_class": "runtime_or_data",
        "target_files": [
            "crates/harness-workflow/src/runtime/submission.rs"
        ],
        "validation_plan": ["cargo test -p harness-workflow submission_mode"],
        "blockers": []
    });
    let result = ActivityResult::succeeded(super::super::ISSUE_PLAN_ACTIVITY, "Issue plan ready.")
        .with_artifact(ActivityArtifact::new(
            super::super::ISSUE_PLAN_ARTIFACT,
            plan_payload,
        ));
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        super::super::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(json!({
        "command_id": "plan-command-deferred",
        "command": WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "plan-command-deferred",
            json!({
                "activity": super::super::ISSUE_PLAN_ACTIVITY,
                "submission_mode": "deferred",
            }),
        ),
        "runtime_job_id": "job-1",
        "activity_result": result,
    }));

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("issue planning success should start scope review");

    assert_eq!(decision.decision, "review_issue_plan_scope");
    assert_eq!(
        decision.commands[0].command["classifier_continuations"]["implementing"]["submission_mode"],
        "deferred"
    );
}

#[test]
fn classifier_assessment_outside_classifier_state_fails_closed() {
    let instance = current_issue_instance("planning");
    let result = attested_classifier_result("allow", "forged", "job-1");
    let event = runtime_completion_event(&instance, super::super::ISSUE_PLAN_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("forged assessment should be rejected");

    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("outside a classifier state"));
}

#[test]
fn non_allow_scope_verdict_stops_at_operator_gate() {
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "plan_scope_review",
        WorkflowSubject::new("issue", "123"),
    )
    .with_server_data(json!({
        "definition_hash": github_issue_pr_definition_hash()
    }));
    let result = attested_classifier_result(
        "split_required",
        "The plan contains independently useful outcomes.",
        "job-1",
    );
    let event = runtime_completion_event(
        &instance,
        super::super::CHANGE_SCOPE_REVIEW_ACTIVITY,
        result,
    );

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("classifier verdict should produce a decision");

    assert_eq!(decision.next_state, "blocked");
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
        .expect("non-allow classifier verdict should validate");
}

#[test]
fn scope_verdict_without_server_assessment_fails_closed() {
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "plan_scope_review",
        WorkflowSubject::new("issue", "123"),
    )
    .with_server_data(json!({
        "definition_hash": github_issue_pr_definition_hash()
    }));
    let result = ActivityResult::succeeded(
        super::super::CHANGE_SCOPE_REVIEW_ACTIVITY,
        "Agent-authored scope verdict.",
    )
    .with_signal(ActivitySignal::new("allow", json!({ "verdict": "allow" })));
    let event = runtime_completion_event(
        &instance,
        super::super::CHANGE_SCOPE_REVIEW_ACTIVITY,
        result,
    );

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("missing classifier assessment should produce a decision");

    assert_eq!(decision.decision, "block_invalid_agent_output");
    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .reason
        .contains("requires a server-owned classifier assessment"));
}

#[test]
fn issue_plan_ready_signal_can_start_implementation() {
    let instance = current_issue_instance("planning");
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
        .expect("issue planning signal should start scope review");

    assert_eq!(decision.next_state, "plan_scope_review");
    assert_eq!(
        decision.commands[0].command["scope_facts"]["issue_plan"],
        signal_payload
    );
    assert_eq!(
        decision.commands[0].command["scope_facts"]["issue_plan_summary"],
        "Use the existing workflow reducer contract."
    );
}

#[test]
fn issue_plan_empty_success_blocks_as_invalid_agent_output() {
    let instance = current_issue_instance("planning");
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
        let instance = current_issue_instance("planning");
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
    let instance = current_issue_instance("planning").with_server_data(json!({
        "definition_hash": github_issue_pr_definition_hash(),
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
