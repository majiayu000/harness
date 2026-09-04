use super::super::EvalVerifyCommandMode;
use super::*;
use crate::runtime::{EvalTrustedVerifier, RuntimeJobStatus, RuntimeKind, WorkflowRuntimeStore};

#[test]
fn eval_run_plan_marks_issue_submission_for_draft_prs() -> anyhow::Result<()> {
    let case = EvalBenchmarkCase {
        case_id: "owner/repo#42".to_string(),
        repo: "owner/repo".to_string(),
        issue: 42,
        base_commit: "abcdef1".to_string(),
        verify_commands: vec!["cargo test -p harness-workflow eval_run".to_string()],
        verify_command_mode: EvalVerifyCommandMode::Argv,
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: None,
        verdict: None,
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())?,
        isolation: EvalIsolationProfile::default(),
    };
    let input = EvalCaseWorkflowInput {
        eval_run_id: "run-1",
        case: &case,
        project_id: "/repo",
        task_id: "eval-task-1",
        additional_prompt: None,
        timeout_secs: 45,
        resource_limits: &case.resource_limits,
    };

    let verification_argv = input.case.verification_command_argv()?;
    let initial = eval_case_initial_instance(input, &verification_argv);
    assert_eq!(initial.id, "eval:run-1:owner/repo#42");
    assert_eq!(initial.definition_id, GITHUB_ISSUE_PR_DEFINITION_ID);
    assert_eq!(initial.data["author_trust_class"], "non_collaborator");
    assert_eq!(initial.data["eval"]["eval_run_id"], "run-1");
    assert_eq!(initial.data["eval"]["branch_prefix"], EVAL_BRANCH_PREFIX);
    assert_eq!(
        initial.data["eval"]["pull_request_mode"],
        EVAL_PR_DRAFT_MODE
    );
    assert_eq!(initial.data["eval"]["isolation"]["tier"], "container");
    assert_eq!(
        initial.data["eval"]["isolation"]["runtime_kind"],
        "remote_host"
    );
    assert_eq!(
        initial.data["eval"]["isolation"]["runtime_profile"],
        "eval-isolated-runtime-host"
    );
    assert_eq!(initial.data["eval"]["isolation"]["lifecycle"], "ephemeral");
    assert_eq!(initial.data["eval"]["isolation"]["cleanup_required"], true);

    let output = build_issue_submission_decision(
        &initial,
        IssueSubmissionDecisionInput {
            task_id: "eval-task-1",
            repo: Some("owner/repo"),
            issue_number: 42,
            labels: &[],
            force_execute: true,
            additional_prompt: Some(EVAL_CASE_DEFAULT_ADDITIONAL_PROMPT),
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );
    let verification_argv = input.case.verification_command_argv()?;
    let decision = with_eval_command_metadata(output.decision, input, &verification_argv);
    let command = &decision.commands[0].command;
    assert_eq!(command["activity"], "implement_issue");
    assert_eq!(command["eval"]["eval_run_id"], "run-1");
    assert_eq!(command["eval"]["timeout_secs"], 45);
    assert_eq!(
        command["eval"]["resource_limits"]["effective"]["wall_time_secs"],
        120
    );
    assert_eq!(command["branch_prefix"], EVAL_BRANCH_PREFIX);
    assert_eq!(command["pull_request_mode"], EVAL_PR_DRAFT_MODE);
    assert_eq!(command["eval"]["isolation"]["tier"], "container");
    assert_eq!(command["eval"]["isolation"]["runtime_kind"], "remote_host");
    assert_eq!(
        command["validation_commands"][0],
        "cargo test -p harness-workflow eval_run"
    );
    assert_eq!(
        command["validation_commands_argv"][0],
        json!(["cargo", "test", "-p", "harness-workflow", "eval_run"])
    );
    Ok(())
}

#[test]
fn eval_run_keeps_trusted_verifier_out_of_agent_visible_commands() -> anyhow::Result<()> {
    let case = EvalBenchmarkCase {
        case_id: "gh1454-scoped-ci-jobs".to_string(),
        repo: "majiayu000/harness".to_string(),
        issue: 1454,
        base_commit: "9c0099ad458e82fd377fd20a8e288a46722762ef".to_string(),
        verify_commands: Vec::new(),
        verify_command_mode: EvalVerifyCommandMode::Argv,
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: None,
        verdict: None,
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())?,
        isolation: EvalIsolationProfile::default(),
    };
    let input = EvalCaseWorkflowInput {
        eval_run_id: "run-trusted",
        case: &case,
        project_id: "/repo",
        task_id: "eval-task-trusted",
        additional_prompt: None,
        timeout_secs: case.timeout_secs,
        resource_limits: &case.resource_limits,
    };
    let verification_argv = case.verification_command_argv()?;

    let initial = eval_case_initial_instance(input, &verification_argv);
    assert_eq!(initial.data["eval"]["verify_commands"], json!([]));
    assert_eq!(
        initial.data["eval"]["verify_commands_argv"],
        json!([EvalTrustedVerifier::Gh1454CiContractV1.validation_argv()])
    );

    let output = build_issue_submission_decision(
        &initial,
        IssueSubmissionDecisionInput {
            task_id: "eval-task-trusted",
            repo: Some("majiayu000/harness"),
            issue_number: 1454,
            labels: &[],
            force_execute: true,
            additional_prompt: Some(EVAL_CASE_DEFAULT_ADDITIONAL_PROMPT),
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );
    let decision = with_eval_command_metadata(output.decision, input, &verification_argv);
    let command = &decision.commands[0].command;
    assert_eq!(command["validation_commands"], json!([]));
    assert_eq!(
        command["validation_commands_argv"],
        json!([EvalTrustedVerifier::Gh1454CiContractV1.validation_argv()])
    );
    Ok(())
}

#[test]
fn eval_run_prompt_preserves_required_draft_pr_constraints() {
    let prompt = eval_case_additional_prompt(Some("Use the small implementation slice."));
    assert!(prompt.contains("open only a draft pull request"));
    assert!(prompt.contains("harness-eval/ branch prefix"));
    assert!(prompt.contains("Use the small implementation slice."));
}

#[test]
fn eval_run_rejects_pending_cases_before_dispatch() {
    let case = EvalBenchmarkCase {
        case_id: "pending-case".to_string(),
        repo: "owner/repo".to_string(),
        issue: 42,
        base_commit: "abcdef1".to_string(),
        verify_commands: vec!["cargo test -p harness-workflow eval_run".to_string()],
        verify_command_mode: EvalVerifyCommandMode::Argv,
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: Some(crate::runtime::eval::manifest::EvalCommitResolution::Pending),
        verdict: Some(crate::runtime::eval::manifest::EvalCaseVerdict::Pending),
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())
            .expect("default resource limits should be valid"),
        isolation: crate::runtime::eval::manifest::EvalIsolationProfile::default(),
    };

    let err = validate_eval_case_replayable(&case).expect_err("pending case should not dispatch");
    assert!(err.to_string().contains("commit_resolution is pending"));
}

#[test]
fn eval_cleanup_summary_requires_zero_remaining_resources() {
    let mut summary = EvalRunCleanupSummary::new("run-1");
    assert!(summary.is_clean());

    summary.active_runtime_jobs = 1;
    assert!(!summary.is_clean());

    summary.active_runtime_jobs = 0;
    summary.orphan_pull_requests = 1;
    assert!(!summary.is_clean());
}

#[tokio::test]
async fn eval_cleanup_cancels_mid_run_workflow_without_runtime_orphans() -> anyhow::Result<()> {
    if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime_store")).await?;
    let case = EvalBenchmarkCase {
        case_id: "owner/repo#42".to_string(),
        repo: "owner/repo".to_string(),
        issue: 42,
        base_commit: "abcdef1".to_string(),
        verify_commands: vec!["cargo test -p harness-workflow eval_cleanup".to_string()],
        verify_command_mode: EvalVerifyCommandMode::Argv,
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: None,
        verdict: None,
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())?,
        isolation: EvalIsolationProfile::default(),
    };
    let outcome = enqueue_eval_case_workflow(
        &store,
        EvalCaseWorkflowInput {
            eval_run_id: "run-cleanup",
            case: &case,
            project_id: dir.path().to_string_lossy().as_ref(),
            task_id: "eval-task-1",
            additional_prompt: None,
            timeout_secs: case.timeout_secs,
            resource_limits: &case.resource_limits,
        },
    )
    .await?;
    assert_eq!(outcome.command_ids.len(), 1);
    let _job = store
        .enqueue_runtime_job_for_pending_command(
            &outcome.command_ids[0],
            RuntimeKind::CodexExec,
            "codex",
            json!({
                "activity": "implement_issue",
                "eval": {
                    "eval_run_id": "run-cleanup",
                    "case_id": case.case_id.clone(),
                }
            }),
            None,
        )
        .await?;

    let summary = cleanup_cancelled_eval_run(
        &store,
        EvalRunCleanupInput {
            eval_run_id: "run-cleanup",
            cases: std::slice::from_ref(&case),
            reason: "operator cancelled eval run",
        },
    )
    .await?;

    assert_eq!(summary.workflows_seen, 1);
    assert_eq!(summary.workflows_cancelled, 1);
    assert_eq!(summary.commands_cancelled, 1);
    assert_eq!(summary.runtime_jobs_cancelled, 1);
    assert!(summary.is_clean());

    let workflow = match store.get_instance(&outcome.plan.workflow_id).await? {
        Some(workflow) => workflow,
        None => panic!("eval workflow should remain as terminal history"),
    };
    assert_eq!(workflow.state, "cancelled");
    assert_eq!(
        workflow.data["eval"]["cleanup"]["reason"],
        "operator cancelled eval run"
    );

    let commands = store.commands_for(&outcome.plan.workflow_id).await?;
    assert!(commands.iter().all(|command| {
        matches!(
            command.status,
            WorkflowCommandStatus::Cancelled | WorkflowCommandStatus::HandledInline
        )
    }));
    let jobs = store
        .runtime_jobs_for_command(&outcome.command_ids[0])
        .await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, RuntimeJobStatus::Cancelled);

    Ok(())
}

#[tokio::test]
async fn eval_run_leaves_dispatch_to_the_runtime_host() -> anyhow::Result<()> {
    if harness_core::config::process_env::var_os("HARNESS_DATABASE_URL").is_none() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime_store")).await?;
    let case = EvalBenchmarkCase {
        case_id: "owner/repo#42".to_string(),
        repo: "owner/repo".to_string(),
        issue: 42,
        base_commit: "abcdef1".to_string(),
        verify_commands: vec!["cargo test -p harness-workflow eval_run".to_string()],
        verify_command_mode: EvalVerifyCommandMode::Argv,
        paths: Vec::new(),
        risk: None,
        evidence: Vec::new(),
        resolution_prs: Vec::new(),
        resolution_commits: Vec::new(),
        commit_resolution: None,
        verdict: None,
        timeout_secs: 120,
        resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
            .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())?,
        isolation: EvalIsolationProfile::default(),
    };

    let outcome = enqueue_eval_case_workflow(
        &store,
        EvalCaseWorkflowInput {
            eval_run_id: "run-1",
            case: &case,
            project_id: dir.path().to_string_lossy().as_ref(),
            task_id: "eval-task-1",
            additional_prompt: None,
            timeout_secs: case.timeout_secs,
            resource_limits: &case.resource_limits,
        },
    )
    .await?;
    assert_eq!(outcome.command_ids.len(), 1);
    let jobs = store
        .runtime_jobs_for_command(&outcome.command_ids[0])
        .await?;
    assert!(jobs.is_empty());
    let command = store.commands_for(&outcome.plan.workflow_id).await?;
    assert_eq!(command[0].status, WorkflowCommandStatus::Pending);
    assert!(command[0].dispatch_barrier.is_none());
    Ok(())
}
