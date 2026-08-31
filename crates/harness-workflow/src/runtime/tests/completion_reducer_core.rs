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
        ))
        // GH-1766: the server verifies the claimed PR before BindPr is minted.
        .with_artifact(verified_pr_binding(77));
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
fn runtime_completion_reducer_compares_pr_identity_and_persists_canonical_url() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/OWNER/REPO/pull/77/files#diff-1"
            }),
        ))
        .with_artifact(verified_pr_binding(77));
    let event = runtime_completion_event(&instance, "implement_issue", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("equivalent pull request identity should bind");

    assert_eq!(decision.decision, "bind_pr");
    assert_eq!(
        decision.commands[0].command["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
    assert!(decision
        .evidence
        .iter()
        .any(|evidence| evidence.kind == "verified_pr_binding"));
}

/// GH-1766 B-006: a PR claim the server never verified is blocked, does not
/// enter `pr_open`, and mints no BindPr command.
#[test]
fn runtime_completion_reducer_blocks_unverified_pull_request_claim() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
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
        .expect("unverified PR claim should still produce a decision");

    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .all(|command| command.command_type != WorkflowCommandType::BindPr));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("blocked decision should validate");
}

/// GH-1766 B-006: a server-recorded verification failure blocks the binding.
#[test]
fn runtime_completion_reducer_blocks_failed_pr_binding_verification() {
    let instance = issue_instance("implementing");
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77"
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_PR_BINDING_VERIFICATION_FAILED,
            json!({ "outcome": "pr_not_open", "detail": "pull request state is `CLOSED`" }),
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
        .expect("failed verification should produce a decision");

    assert_eq!(decision.next_state, "blocked");
    assert!(decision.reason.contains("pr_binding_verification_failed"));
    assert!(decision
        .commands
        .iter()
        .all(|command| command.command_type != WorkflowCommandType::BindPr));
}

#[test]
fn runtime_completion_reducer_finishes_merge_pr_with_merged_pull_request_artifact() {
    let instance = issue_instance("merging").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
    }));
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
    )
    .with_artifact(ActivityArtifact::new(
        crate::runtime::completion_evidence::ARTIFACT_MERGE_COMPLETION_VERIFICATION,
        json!({
            "schema": crate::runtime::completion_evidence::MERGE_COMPLETION_VERIFICATION_SCHEMA,
            "verified": true,
            "observed_merged": true,
            "repo": "owner/repo",
            "pr_number": 77
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
fn runtime_completion_reducer_rejects_verified_merge_with_mismatched_pr_url() {
    let instance = issue_instance("merging").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
    }));
    let result = ActivityResult::succeeded("merge_pr", "PR was merged.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/attacker/repo/pull/77",
                "merged": true
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_MERGE_COMPLETION_VERIFICATION,
            json!({
                "schema": crate::runtime::completion_evidence::MERGE_COMPLETION_VERIFICATION_SCHEMA,
                "verified": true,
                "observed_merged": true,
                "repo": "owner/repo",
                "pr_number": 77
            }),
        ));
    let event = runtime_completion_event(&instance, "merge_pr", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("mismatched URL should remain auditable");
    let rejection = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect_err("a verification for another URL must not satisfy terminal trust");

    assert_eq!(
        rejection.kind,
        WorkflowDecisionRejectionKind::InsufficientEvidenceTrust
    );
}

#[test]
fn runtime_completion_reducer_rejects_unverified_merge_completion() {
    let instance = issue_instance("merging").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
    }));
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
        .expect("agent-reported merge should produce an auditable decision");

    assert_eq!(decision.decision, "record_pr_merged");
    let terminal_evidence = decision
        .evidence
        .iter()
        .find(|evidence| evidence.kind == "github_terminal_evidence")
        .expect("terminal evidence should be retained for audit");
    assert_eq!(
        terminal_evidence.provenance,
        harness_core::claim_trust::ClaimProvenance::self_declared()
    );
    let rejection = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect_err("an agent-reported merge must not satisfy terminal trust");
    assert_eq!(
        rejection.kind,
        WorkflowDecisionRejectionKind::InsufficientEvidenceTrust
    );
}

#[test]
fn runtime_completion_reducer_rejects_server_merge_verification_waiver() {
    let instance = issue_instance("merging").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
    }));
    let result = ActivityResult::succeeded("merge_pr", "PR was merged.")
        .with_artifact(ActivityArtifact::new(
            "pull_request",
            json!({
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
                "merged": true
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            crate::runtime::completion_evidence::ARTIFACT_MERGE_COMPLETION_VERIFICATION,
            json!({
                "schema": crate::runtime::completion_evidence::MERGE_COMPLETION_VERIFICATION_SCHEMA,
                "verified": false,
                "observed_merged": false,
                "outcome": "verification_waived",
                "verification_source": "server_configuration",
                "repo": "owner/repo",
                "pr_number": 77
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
        .expect("unverified merge should produce an auditable decision");

    assert_eq!(decision.decision, "record_pr_merged");
    let terminal_evidence = decision
        .evidence
        .iter()
        .find(|evidence| evidence.kind == "github_terminal_evidence")
        .expect("terminal evidence");
    assert_eq!(
        terminal_evidence.provenance,
        harness_core::claim_trust::ClaimProvenance::self_declared()
    );
    let rejection = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect_err("a configuration waiver must not satisfy terminal trust");
    assert_eq!(
        rejection.kind,
        WorkflowDecisionRejectionKind::InsufficientEvidenceTrust
    );
}

#[test]
fn runtime_completion_reducer_rejects_merge_verification_without_matching_repo() {
    let instances = [
        issue_instance("merging"),
        issue_instance("merging").with_server_data(json!({
            "repo": "owner/repo",
            "pr_number": 77,
        })),
    ];

    for instance in instances {
        let result = ActivityResult::succeeded("merge_pr", "PR was merged.")
            .with_artifact(ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 77,
                    "pr_url": "https://github.com/owner/repo/pull/77",
                    "state": "merged",
                    "merged": true
                }),
            ))
            .with_artifact(ActivityArtifact::new(
                crate::runtime::completion_evidence::ARTIFACT_MERGE_COMPLETION_VERIFICATION,
                json!({
                    "schema": crate::runtime::completion_evidence::MERGE_COMPLETION_VERIFICATION_SCHEMA,
                    "verified": true,
                    "observed_merged": true,
                    "repo": "other/repo",
                    "pr_number": 77
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
            .expect("unbound verification should produce an auditable decision");
        let terminal_evidence = decision
            .evidence
            .iter()
            .find(|evidence| evidence.kind == "github_terminal_evidence")
            .expect("terminal evidence should be retained for audit");
        assert_eq!(
            terminal_evidence.provenance,
            harness_core::claim_trust::ClaimProvenance::self_declared()
        );
        assert_eq!(
            DecisionValidator::github_issue_pr()
                .validate(
                    &instance,
                    &decision,
                    &ValidationContext::new("runtime-1", Utc::now()),
                )
                .expect_err("repository identity must match server verification")
                .kind,
            WorkflowDecisionRejectionKind::InsufficientEvidenceTrust
        );
    }
}

#[test]
fn runtime_completion_reducer_keeps_agent_reported_issue_closure_self_declared() {
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
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(999));
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
    let terminal_evidence = decision
        .evidence
        .iter()
        .find(|evidence| evidence.kind == "github_terminal_evidence")
        .expect("terminal evidence should be retained for audit");
    assert_eq!(
        terminal_evidence.provenance,
        harness_core::claim_trust::ClaimProvenance::self_declared()
    );
    let rejection = DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect_err("agent-reported closure must not satisfy terminal trust");
    assert_eq!(
        rejection.kind,
        WorkflowDecisionRejectionKind::InsufficientEvidenceTrust
    );
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
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));
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
        artifacts: vec![crate::runtime::completion_evidence::verified_issue_state_for_test(123)],
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
        artifacts: vec![crate::runtime::completion_evidence::verified_issue_state_for_test(123)],
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
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));
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
fn runtime_completion_reducer_finishes_closed_issue_during_local_review() {
    let instance = issue_instance("local_review_gate");
    let result = ActivityResult::succeeded(
        LOCAL_REVIEW_ACTIVITY,
        "Issue was closed while local review was running.",
    )
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));
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
        .expect("verified issue closure should terminate local review");

    assert_eq!(decision.decision, "finish_closed_issue");
    assert_eq!(decision.next_state, "done");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("server-verified issue closure should validate during local review");
}

#[test]
fn runtime_completion_reducer_finishes_from_server_merged_pr_snapshot() {
    let instance = issue_instance("awaiting_feedback").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server snapshot observed a merged pull request.",
    )
    .with_artifact(ActivityArtifact::new(
        crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "snapshot_source": "server_github_graphql",
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "state": "MERGED",
            "head_oid": "abc123",
            "merge_commit_sha": "def456",
            "observed_at": "2026-08-31T00:00:00Z",
        }),
    ));
    let event = runtime_completion_event(&instance, PR_FEEDBACK_INSPECT_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("server-observed PR merge should terminate feedback processing");

    assert_eq!(decision.decision, "finish_server_observed_pr_merge");
    assert_eq!(decision.next_state, "done");
    assert_eq!(decision.commands[0].command_type, WorkflowCommandType::MarkDone);
}

#[test]
fn runtime_completion_reducer_does_not_finish_failed_merge_from_merged_snapshot() {
    let instance = issue_instance("merging").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::failed(
        "merge_pr",
        "Server-side merge completion verification rejected a stale pull request head.",
        "observed head did not match expected_head_sha",
    )
    .with_error_kind(ActivityErrorKind::Fatal)
    .with_artifact(ActivityArtifact::new(
        crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "snapshot_source": "server_github_graphql",
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "state": "MERGED",
            "head_oid": "stale-head",
            "observed_at": "2026-08-31T00:00:00Z",
        }),
    ));
    let event = runtime_completion_event(&instance, "merge_pr", result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("failed merge verification should produce a failure decision");

    assert_eq!(decision.decision, "fail_after_runtime_activity");
    assert_eq!(decision.next_state, "failed");
}

#[test]
fn runtime_completion_reducer_cancels_from_server_closed_pr_snapshot() {
    let instance = issue_instance("local_review_gate").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server snapshot observed a closed pull request.",
    )
    .with_artifact(ActivityArtifact::new(
        crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "snapshot_source": "server_github_graphql",
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "state": "CLOSED",
            "head_oid": "abc123",
            "observed_at": "2026-08-31T00:00:00Z",
        }),
    ));
    let event = runtime_completion_event(&instance, PR_FEEDBACK_INSPECT_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("server-observed PR closure should terminate feedback processing");

    assert_eq!(decision.decision, "cancel_server_observed_closed_pr");
    assert_eq!(decision.next_state, "cancelled");
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::MarkCancelled
    );
}

#[test]
fn runtime_completion_reducer_requires_complete_bound_pr_identity_for_terminal_snapshot() {
    let instance = issue_instance("awaiting_feedback").with_server_data(json!({
        "pr_number": 77,
    }));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server snapshot observed a merged pull request.",
    )
    .with_artifact(ActivityArtifact::new(
        crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "snapshot_source": "server_github_graphql",
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "state": "MERGED",
        }),
    ));
    let event = runtime_completion_event(&instance, PR_FEEDBACK_INSPECT_ACTIVITY, result);

    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("invalid feedback completion should produce a decision");

    assert_ne!(decision.decision, "finish_server_observed_pr_merge");
    assert_ne!(decision.next_state, "done");
}

#[test]
fn runtime_completion_reducer_validates_late_server_merge_from_blocked_state() {
    let instance = issue_instance("blocked").with_server_data(json!({
        "repo": "owner/repo",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
    }));
    let result = ActivityResult::succeeded(
        PR_FEEDBACK_INSPECT_ACTIVITY,
        "Server snapshot observed a late pull request merge.",
    )
    .with_artifact(ActivityArtifact::new(
        crate::runtime::SERVER_PR_SNAPSHOT_ARTIFACT,
        json!({
            "snapshot_source": "server_github_graphql",
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "state": "MERGED",
        }),
    ));
    let event = runtime_completion_event(&instance, PR_FEEDBACK_INSPECT_ACTIVITY, result);
    let decision = reduce_runtime_job_completed(&instance, &event)
        .expect("event should parse")
        .expect("late server merge should terminate a blocked workflow");

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("runtime-observed merge should validate from blocked");
    assert_eq!(decision.next_state, "done");
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
            ))
            .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));
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
fn runtime_completion_reducer_blocks_same_number_wrong_repo_bind_pr() {
    let instance = issue_instance("implementing");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "bind_pr",
        "pr_open",
        "Bind an unverified PR.",
    )
    .with_command(WorkflowCommand::bind_pr(
        77,
        "https://github.com/attacker/repo/pull/77",
        "forged-bind-pr",
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
                "pr_url": "https://github.com/attacker/repo/pull/77"
            }),
        ))
        .with_artifact(verified_pr_binding(77));
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
        .expect("wrong-repository PR binding should block");

    assert_eq!(decision.next_state, "blocked");
    assert!(decision
        .commands
        .iter()
        .all(|command| command.command_type != WorkflowCommandType::BindPr));
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("fallback PR binding should validate");
}

#[test]
fn runtime_completion_reducer_canonicalizes_verified_structured_bind_pr_url() {
    let instance = issue_instance("implementing");
    let proposed_decision = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "bind_pr",
        "pr_open",
        "Bind the verified PR.",
    )
    .with_command(WorkflowCommand::bind_pr(
        77,
        "https://github.com/OWNER/REPO/pull/77/files?diff=split#discussion",
        "verified-bind-pr",
    ));
    let result = ActivityResult::succeeded("implement_issue", "Implementation completed.")
        .with_artifact(ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&proposed_decision).expect("decision should serialize"),
        ))
        .with_artifact(verified_pr_binding(77));
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
        .expect("verified structured binding should be accepted");

    assert_eq!(decision.next_state, "pr_open");
    let bind_pr = decision
        .commands
        .iter()
        .find(|command| command.command_type == WorkflowCommandType::BindPr)
        .expect("bind PR command");
    assert_eq!(
        bind_pr.command["pr_url"],
        "https://github.com/owner/repo/pull/77"
    );
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
    )
    .with_provenance(harness_core::claim_trust::ClaimProvenance::human_approved(
        "forged-approver",
        "forged-approval",
    )))
    .high_confidence();
    let result = ActivityResult::succeeded(
        "inspect_pr_feedback",
        "No actionable PR feedback was found.",
    )
    .with_artifact(ActivityArtifact::new(
        "workflow_decision",
        serde_json::to_value(&proposed_decision).expect("decision should serialize"),
    ))
    .with_signal(ActivitySignal::new(
        "NoFeedbackFound",
        json!({ "pr_number": 77 }),
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
    let agent_evidence = decision
        .evidence
        .iter()
        .find(|evidence| evidence.kind == "pr_feedback")
        .expect("agent evidence should remain available at self-declared trust");
    assert_eq!(
        agent_evidence.provenance,
        harness_core::claim_trust::ClaimProvenance::self_declared()
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )
        .expect("structured workflow decision should validate");
}
