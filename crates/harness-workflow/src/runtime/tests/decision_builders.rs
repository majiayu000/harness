#[test]
fn issue_submission_decision_starts_discovered_issue_planning() {
    let labels = vec!["bug".to_string(), "p1".to_string()];
    let instance = issue_instance("discovered");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-1",
            repo: Some("owner/repo"),
            issue_number: 123,
            labels: &labels,
            force_execute: false,
            additional_prompt: Some("prefer a minimal patch"),
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );

    assert_eq!(output.action, IssueSubmissionWorkflowAction::RunPlanning);
    assert_eq!(output.decision.decision, "submit_issue");
    assert_eq!(output.decision.next_state, "planning");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("plan_issue")
    );
    assert_eq!(
        output.decision.commands[0].dedupe_key,
        "issue-submit:owner/repo:issue:123:task:task-1:plan"
    );
    assert_eq!(
        output.decision.commands[0].command["additional_prompt"],
        "prefer a minimal patch"
    );
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("issue submission decision should validate");
}

#[test]
fn issue_submission_decision_can_reopen_failed_issue_when_requested() {
    let labels = Vec::new();
    let instance = issue_instance("failed");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-2",
            repo: None,
            issue_number: 124,
            labels: &labels,
            force_execute: true,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );

    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()).allow_terminal_reopen(),
        )
        .expect("explicit issue submission should reopen failed workflows");
}

#[test]
fn issue_submission_decision_can_reopen_terminal_issue_for_planning() {
    let labels = Vec::new();
    for state in ["failed", "cancelled"] {
        let instance = issue_instance(state);
        let output = build_issue_submission_decision(
            &instance,
            IssueSubmissionDecisionInput {
                task_id: "task-terminal",
                repo: Some("owner/repo"),
                issue_number: 124,
                labels: &labels,
                force_execute: false,
                additional_prompt: None,
                depends_on: &[],
                dependencies_blocked: false,
                remote_fact_hash: None,
                submission_mode: SubmissionMode::Immediate,
                candidate_fanout: None,
            },
        );

        assert_eq!(output.action, IssueSubmissionWorkflowAction::RunPlanning);
        assert_eq!(output.decision.next_state, "planning");
        assert_eq!(
            output.decision.commands[0].activity_name(),
            Some("plan_issue")
        );
        let validation = DecisionValidator::github_issue_pr().validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()).allow_terminal_reopen(),
        );
        assert!(
            validation.is_ok(),
            "terminal issue in {state} should reopen for planning: {validation:?}"
        );
    }
}

#[test]
fn issue_submission_decision_waits_for_dependencies_without_runtime_command() {
    let labels = Vec::new();
    let depends_on = vec!["task-upstream".to_string()];
    let instance = issue_instance("discovered");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-3",
            repo: Some("owner/repo"),
            issue_number: 125,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &depends_on,
            dependencies_blocked: true,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );

    assert_eq!(output.decision.next_state, "awaiting_dependencies");
    assert!(output.decision.commands.is_empty());
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocked issue submission should validate without dispatching");
}

#[test]
fn issue_submission_decision_releases_dependencies_to_planning() {
    let labels = Vec::new();
    let instance = issue_instance("awaiting_dependencies");
    let output = build_issue_submission_decision(
        &instance,
        IssueSubmissionDecisionInput {
            task_id: "task-4",
            repo: Some("owner/repo"),
            issue_number: 126,
            labels: &labels,
            force_execute: false,
            additional_prompt: None,
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );

    assert_eq!(output.action, IssueSubmissionWorkflowAction::RunPlanning);
    assert_eq!(output.decision.next_state, "planning");
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some("plan_issue")
    );
    let validation = DecisionValidator::github_issue_pr().validate(
        &instance,
        &output.decision,
        &ValidationContext::new("workflow-policy", Utc::now()),
    );
    assert!(
        validation.is_ok(),
        "dependency release should allow planning: {validation:?}"
    );
}

#[test]
fn prompt_submission_decision_starts_runtime_implementation() {
    let instance = prompt_task_instance("submitted");
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: "task-prompt-1",
            prompt: "Fix the flaky login test.",
            prompt_ref: "prompt-ref-1",
            source: None,
            external_id: Some("manual-prompt-1"),
            depends_on: &[],
            dependencies_blocked: false,
        },
    );

    assert_eq!(output.decision.decision, "submit_prompt");
    assert_eq!(output.decision.next_state, "implementing");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some(PROMPT_TASK_IMPLEMENT_ACTIVITY)
    );
    assert_eq!(
        output.decision.commands[0].command["prompt_ref"],
        "prompt-ref-1"
    );
    assert!(output.decision.commands[0].command.get("prompt").is_none());
    assert_eq!(
        output.decision.commands[0].command["prompt_chars"],
        "Fix the flaky login test.".chars().count()
    );
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("prompt submission decision should validate");
}

#[test]
fn prompt_submission_decision_waits_for_dependencies_without_runtime_command() {
    let depends_on = vec!["task-upstream".to_string()];
    let instance = prompt_task_instance("submitted");
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: "task-prompt-2",
            prompt: "Refactor the settings panel.",
            prompt_ref: "prompt-ref-2",
            source: None,
            external_id: None,
            depends_on: &depends_on,
            dependencies_blocked: true,
        },
    );

    assert_eq!(output.decision.next_state, "awaiting_dependencies");
    assert!(output.decision.commands.is_empty());
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocked prompt submission should validate without dispatching");
}

#[test]
fn prompt_submission_decision_reopens_blocked_prompt_task() {
    let instance = prompt_task_instance("blocked");
    let output = build_prompt_submission_decision(
        &instance,
        PromptSubmissionDecisionInput {
            task_id: "task-prompt-retry",
            prompt: "Retry the prompt task after payload loss.",
            prompt_ref: "prompt-ref-retry",
            source: None,
            external_id: Some("manual-prompt-retry"),
            depends_on: &[],
            dependencies_blocked: false,
        },
    );

    assert_eq!(output.decision.next_state, "implementing");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].activity_name(),
        Some(PROMPT_TASK_IMPLEMENT_ACTIVITY)
    );
    DecisionValidator::prompt_task()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocked prompt task resubmission should validate");
}

#[test]
fn plan_issue_decision_runs_replan_when_policy_allows() {
    let instance = issue_instance("implementing");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan missed rollback",
            force_execute: false,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: false,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::RunReplan);
    assert_eq!(output.decision.decision, "run_replan");
    assert_eq!(output.decision.next_state, "replanning");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("replan decision should validate");
}

#[test]
fn plan_issue_decision_replans_after_shadow_issue_submission() {
    let instance = issue_instance("scheduled");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan missed rollback",
            force_execute: false,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: false,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::RunReplan);
    assert_eq!(output.decision.next_state, "replanning");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("shadow-submitted issues should be allowed to replan");
}

#[test]
fn plan_issue_decision_force_execute_continues_implementation() {
    let instance = issue_instance("implementing");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan missed rollback",
            force_execute: true,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: false,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::ForceContinue);
    assert_eq!(output.decision.decision, "continue_force_execute");
    assert_eq!(output.decision.next_state, "implementing");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("force continue decision should validate");
}

#[test]
fn plan_issue_force_continue_after_shadow_issue_submission() {
    let instance = issue_instance("scheduled");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan missed rollback",
            force_execute: true,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: false,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::ForceContinue);
    assert_eq!(output.decision.next_state, "implementing");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("shadow-submitted issues should allow force continue");
}

#[test]
fn plan_issue_decision_blocks_repeated_replan() {
    let instance = issue_instance("implementing");
    let output = build_plan_issue_decision(
        &instance,
        PlanIssueDecisionInput {
            task_id: "task-1",
            plan_issue: "plan still invalid",
            force_execute: false,
            auto_replan_on_plan_issue: true,
            replan_already_attempted: true,
            turn_budget_exhausted: false,
        },
    );

    assert_eq!(output.action, PlanIssueWorkflowAction::Block);
    assert_eq!(output.decision.decision, "block_replan_loop");
    assert_eq!(output.decision.next_state, "blocked");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("block decision should validate");
}

#[test]
fn decision_validator_lists_allowed_transitions_from_state() {
    let validator = DecisionValidator::github_issue_pr();
    let rules = validator
        .transition_rules_from("ready_to_merge")
        .collect::<Vec<_>>();

    assert!(rules.iter().any(|rule| rule.to_state == "done"
        && rule
            .allowed_commands
            .contains(&WorkflowCommandType::MarkDone)));
    assert!(rules.iter().any(|rule| rule.to_state == "blocked"
        && rule
            .allowed_commands
            .contains(&WorkflowCommandType::MarkBlocked)));
}

#[test]
fn pr_detected_decision_binds_pr_from_implementation() {
    let instance = issue_instance("implementing");
    let output = build_pr_detected_decision(
        &instance,
        PrDetectedDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::BindPr);
    assert_eq!(output.decision.decision, "bind_pr");
    assert_eq!(output.decision.next_state, "pr_open");
    assert_eq!(output.decision.commands.len(), 1);
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("PR binding decision should validate");
}

#[test]
fn pr_detected_decision_binds_pr_after_shadow_issue_submission() {
    let instance = issue_instance("scheduled");
    let output = build_pr_detected_decision(
        &instance,
        PrDetectedDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: "https://github.com/owner/repo/pull/77",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::BindPr);
    assert_eq!(output.decision.next_state, "pr_open");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("shadow-submitted issues should bind a produced PR");
}

#[test]
fn pr_feedback_decision_addresses_blocking_feedback() {
    let instance = issue_instance("awaiting_feedback");
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::BlockingFeedback,
            summary: "Reviewer found blocking feedback.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::AddressFeedback);
    assert_eq!(output.decision.decision, "address_pr_feedback");
    assert_eq!(output.decision.next_state, "addressing_feedback");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("blocking feedback decision should validate");
}

#[test]
fn pr_feedback_sweep_decision_starts_child_workflow() {
    let instance = issue_instance("awaiting_feedback");
    let output = build_pr_feedback_sweep_decision(
        &instance,
        PrFeedbackSweepDecisionInput {
            dedupe_key: "pr-feedback-sweep:123:77",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            issue_number: Some(123),
            repo: Some("owner/repo"),
            summary: "Runtime workflow requested a PR feedback sweep.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::SweepFeedback);
    assert_eq!(output.decision.decision, "sweep_pr_feedback");
    assert_eq!(output.decision.next_state, "awaiting_feedback");
    assert_eq!(output.decision.commands.len(), 1);
    assert_eq!(
        output.decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        output.decision.commands[0].command["definition_id"],
        PR_FEEDBACK_DEFINITION_ID
    );
    assert_eq!(
        output.decision.commands[0].command["child_activity"],
        PR_FEEDBACK_INSPECT_ACTIVITY
    );
    assert_eq!(output.decision.commands[0].command["pr_number"], 77);
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("PR feedback sweep decision should validate");
}

#[test]
fn pr_feedback_decision_waits_when_no_actionable_feedback_exists() {
    let instance = issue_instance("awaiting_feedback");
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: None,
            outcome: PrFeedbackOutcome::NoActionableFeedback,
            summary: "Review bot has not produced actionable feedback yet.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::AwaitFeedback);
    assert_eq!(output.decision.decision, "wait_for_pr_feedback");
    assert_eq!(output.decision.next_state, "awaiting_feedback");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("wait decision should validate");
}

#[test]
fn pr_feedback_decision_starts_quality_gate_before_ready_to_merge() {
    let instance = issue_instance("awaiting_feedback");
    let output = build_pr_feedback_decision(
        &instance,
        PrFeedbackDecisionInput {
            task_id: "task-1",
            pr_number: 77,
            pr_url: Some("https://github.com/owner/repo/pull/77"),
            outcome: PrFeedbackOutcome::ReadyToMerge,
            summary: "Reviewer approved and validation passed.",
        },
    );

    assert_eq!(output.action, PrFeedbackWorkflowAction::RequestQualityGate);
    assert_eq!(output.decision.decision, "start_quality_gate");
    assert_eq!(output.decision.next_state, "quality_gate_pending");
    assert_eq!(
        output.decision.commands[0].command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        output.decision.commands[0].command["definition_id"],
        QUALITY_GATE_DEFINITION_ID
    );
    assert_eq!(output.decision.commands[0].command["subject_key"], "pr:77");
    DecisionValidator::github_issue_pr()
        .validate(
            &instance,
            &output.decision,
            &ValidationContext::new("workflow-policy", Utc::now()),
        )
        .expect("quality gate request decision should validate");
}
