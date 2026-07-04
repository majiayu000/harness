use super::manifest::EvalBenchmarkCase;
use crate::runtime::{
    build_issue_submission_decision, IssueSubmissionDecisionInput, RuntimeCommandDispatcher,
    RuntimeJobStatus, RuntimeProfile, SubmissionMode, ValidationContext, WorkflowCommand,
    WorkflowCommandStatus, WorkflowCommandType, WorkflowDecision, WorkflowDecisionTransition,
    WorkflowDefinition, WorkflowEvidence, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};
use chrono::Utc;
use serde_json::{json, Value};

pub const EVAL_BRANCH_PREFIX: &str = "harness-eval/";
pub const EVAL_PR_DRAFT_MODE: &str = "draft";
pub const EVAL_RUN_DEFINITION_SOURCE: &str = "runtime_eval";

#[derive(Debug, Clone, PartialEq)]
pub struct EvalCaseWorkflowPlan {
    pub eval_run_id: String,
    pub case_id: String,
    pub workflow_id: String,
    pub initial_instance: WorkflowInstance,
    pub submitted_instance: WorkflowInstance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalCaseEnqueueOutcome {
    pub plan: EvalCaseWorkflowPlan,
    pub command_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvalCaseDispatchOutcome {
    pub enqueue: EvalCaseEnqueueOutcome,
    pub dispatched_jobs: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct EvalCaseWorkflowInput<'a> {
    pub eval_run_id: &'a str,
    pub case: &'a EvalBenchmarkCase,
    pub project_id: &'a str,
    pub task_id: &'a str,
    pub additional_prompt: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct EvalRunCleanupInput<'a> {
    pub eval_run_id: &'a str,
    pub cases: &'a [EvalBenchmarkCase],
    pub reason: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRunCleanupSummary {
    pub eval_run_id: String,
    pub workflows_seen: usize,
    pub workflows_cancelled: usize,
    pub commands_cancelled: usize,
    pub runtime_jobs_cancelled: usize,
    pub active_workflows: usize,
    pub active_commands: usize,
    pub active_runtime_jobs: usize,
    pub orphan_workspaces: usize,
    pub orphan_pull_requests: usize,
    /// Eval runs reuse the workflow runtime schema and must not create per-run schemas.
    pub orphan_schemas: usize,
    pub cleanup_failures: Vec<EvalRunCleanupFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalRunCleanupFailure {
    pub case_id: String,
    pub workflow_id: String,
    pub step: String,
    pub error: String,
}

impl EvalRunCleanupSummary {
    fn new(eval_run_id: impl Into<String>) -> Self {
        Self {
            eval_run_id: eval_run_id.into(),
            workflows_seen: 0,
            workflows_cancelled: 0,
            commands_cancelled: 0,
            runtime_jobs_cancelled: 0,
            active_workflows: 0,
            active_commands: 0,
            active_runtime_jobs: 0,
            orphan_workspaces: 0,
            orphan_pull_requests: 0,
            orphan_schemas: 0,
            cleanup_failures: Vec::new(),
        }
    }

    pub fn is_clean(&self) -> bool {
        self.active_workflows == 0
            && self.active_commands == 0
            && self.active_runtime_jobs == 0
            && self.orphan_workspaces == 0
            && self.orphan_pull_requests == 0
            && self.orphan_schemas == 0
            && self.cleanup_failures.is_empty()
    }

    fn record_failure(
        &mut self,
        case_id: &str,
        workflow_id: &str,
        step: &str,
        error: impl std::fmt::Display,
    ) {
        self.cleanup_failures.push(EvalRunCleanupFailure {
            case_id: case_id.to_string(),
            workflow_id: workflow_id.to_string(),
            step: step.to_string(),
            error: error.to_string(),
        });
    }
}

pub async fn enqueue_eval_case_workflow(
    store: &WorkflowRuntimeStore,
    input: EvalCaseWorkflowInput<'_>,
) -> anyhow::Result<EvalCaseEnqueueOutcome> {
    store
        .upsert_definition(
            &WorkflowDefinition::new(GITHUB_ISSUE_PR_DEFINITION_ID, 1, "GitHub issue PR workflow")
                .with_source_path(EVAL_RUN_DEFINITION_SOURCE),
        )
        .await?;

    let initial_instance = eval_case_initial_instance(input);
    let additional_prompt = eval_case_additional_prompt(input.additional_prompt);
    let output = build_issue_submission_decision(
        &initial_instance,
        IssueSubmissionDecisionInput {
            task_id: input.task_id,
            repo: Some(&input.case.repo),
            issue_number: input.case.issue,
            labels: &[],
            force_execute: true,
            additional_prompt: Some(&additional_prompt),
            depends_on: &[],
            dependencies_blocked: false,
            remote_fact_hash: None,
            submission_mode: SubmissionMode::Immediate,
            candidate_fanout: None,
        },
    );
    let decision = with_eval_command_metadata(output.decision, input);
    let validator = crate::runtime::DecisionValidator::github_issue_pr();
    validator.validate(
        &initial_instance,
        &decision,
        &ValidationContext::new("eval-run", chrono::Utc::now()),
    )?;

    let mut submitted_instance = initial_instance.clone();
    submitted_instance.state = decision.next_state.clone();
    submitted_instance.version = submitted_instance.version.saturating_add(1);
    submitted_instance.data = eval_case_submitted_data(input, &decision.decision);
    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: &initial_instance.state,
            create_if_missing: Some(&initial_instance),
            event_type: "EvalCaseSubmitted",
            source: "eval-run",
            payload: json!({
                "eval_run_id": input.eval_run_id,
                "case_id": input.case.case_id,
                "issue": input.case.issue,
                "repo": input.case.repo,
            }),
            decision: &decision,
            final_instance: &submitted_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "eval case workflow {} was not in the expected `{}` state",
                initial_instance.id,
                initial_instance.state
            )
        })?;
    let command_ids = store
        .commands_for(&submitted_instance.id)
        .await?
        .into_iter()
        .filter(|command| command.decision_id.as_deref() == Some(record.id.as_str()))
        .map(|command| command.id)
        .collect();

    Ok(EvalCaseEnqueueOutcome {
        plan: EvalCaseWorkflowPlan {
            eval_run_id: input.eval_run_id.to_string(),
            case_id: input.case.case_id.clone(),
            workflow_id: submitted_instance.id.clone(),
            initial_instance,
            submitted_instance,
        },
        command_ids,
    })
}

pub async fn dispatch_eval_case_workflow(
    store: &WorkflowRuntimeStore,
    runtime_profile: RuntimeProfile,
    input: EvalCaseWorkflowInput<'_>,
) -> anyhow::Result<EvalCaseDispatchOutcome> {
    let enqueue = enqueue_eval_case_workflow(store, input).await?;
    let outcomes = RuntimeCommandDispatcher::new(store, runtime_profile)
        .dispatch_pending()
        .await?;
    let dispatched_jobs = outcomes
        .into_iter()
        .filter(|outcome| {
            matches!(
                outcome,
                crate::runtime::CommandDispatchOutcome::Enqueued { .. }
                    | crate::runtime::CommandDispatchOutcome::AlreadyDispatched { .. }
            )
        })
        .count();
    Ok(EvalCaseDispatchOutcome {
        enqueue,
        dispatched_jobs,
    })
}

pub async fn cleanup_cancelled_eval_run(
    store: &WorkflowRuntimeStore,
    input: EvalRunCleanupInput<'_>,
) -> anyhow::Result<EvalRunCleanupSummary> {
    let eval_run_id = input.eval_run_id.trim();
    if eval_run_id.is_empty() {
        anyhow::bail!("eval_run_id must not be empty");
    }
    let reason = input.reason.trim();
    if reason.is_empty() {
        anyhow::bail!("eval cleanup reason must not be empty");
    }

    let mut summary = EvalRunCleanupSummary::new(eval_run_id);
    for case in input.cases {
        let workflow_id = eval_case_workflow_id(eval_run_id, &case.case_id);
        let instance = match store.get_instance(&workflow_id).await {
            Ok(Some(instance)) => instance,
            Ok(None) => continue,
            Err(error) => {
                summary.record_failure(&case.case_id, &workflow_id, "load_workflow", error);
                continue;
            }
        };
        summary.workflows_seen += 1;

        let commands = match store.commands_for(&workflow_id).await {
            Ok(commands) => commands,
            Err(error) => {
                summary.record_failure(&case.case_id, &workflow_id, "load_commands", error);
                continue;
            }
        };
        for command in commands {
            if !active_command_status(command.status) {
                continue;
            }
            match store
                .cancel_command_and_unfinished_runtime_jobs(
                    &command.id,
                    command.command.runtime_activity_key(),
                    reason,
                )
                .await
            {
                Ok(cancelled_jobs) => {
                    summary.commands_cancelled += 1;
                    summary.runtime_jobs_cancelled += cancelled_jobs;
                }
                Err(error) => {
                    summary.record_failure(&case.case_id, &workflow_id, "cancel_command", error);
                }
            }
        }

        let mut final_instance = instance.clone();
        if !instance.is_terminal() {
            match cancel_eval_workflow_instance(store, &instance, eval_run_id, case, reason).await {
                Ok(Some(cancelled_instance)) => {
                    summary.workflows_cancelled += 1;
                    final_instance = cancelled_instance;
                }
                Ok(None) => {}
                Err(error) => {
                    summary.record_failure(&case.case_id, &workflow_id, "cancel_workflow", error);
                }
            }
        };

        if let Err(error) =
            collect_remaining_eval_resources(store, &workflow_id, &final_instance, &mut summary)
                .await
        {
            summary.record_failure(
                &case.case_id,
                &workflow_id,
                "collect_remaining_resources",
                error,
            );
        }
    }

    Ok(summary)
}

async fn cancel_eval_workflow_instance(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    eval_run_id: &str,
    case: &EvalBenchmarkCase,
    reason: &str,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let observed_state = instance.state.clone();
    let mut final_instance = instance.clone();
    final_instance.state = "cancelled".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data =
        eval_cleanup_data(final_instance.data, eval_run_id, &case.case_id, reason);

    let decision = WorkflowDecision::new(
        &instance.id,
        &observed_state,
        "cancel_eval_run",
        "cancelled",
        reason,
    )
    .with_evidence(WorkflowEvidence::new(
        "eval_cleanup",
        format!("Eval run {eval_run_id} was cancelled before completion."),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::MarkCancelled,
        format!("eval-cleanup:{eval_run_id}:{}", case.case_id),
        json!({
            "reason": reason,
            "eval_run_id": eval_run_id,
            "case_id": case.case_id,
        }),
    ))
    .high_confidence();

    crate::runtime::DecisionValidator::github_issue_pr().validate(
        instance,
        &decision,
        &ValidationContext::new("eval-cleanup", Utc::now()),
    )?;

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: &observed_state,
            create_if_missing: None,
            event_type: "EvalRunCancelled",
            source: "eval-cleanup",
            payload: json!({
                "eval_run_id": eval_run_id,
                "case_id": case.case_id,
                "reason": reason,
            }),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?;
    Ok(record.map(|_| final_instance))
}

async fn collect_remaining_eval_resources(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    instance: &WorkflowInstance,
    summary: &mut EvalRunCleanupSummary,
) -> anyhow::Result<()> {
    if !instance.is_terminal() {
        summary.active_workflows += 1;
    }
    if instance
        .data
        .pointer("/eval/workspace_path")
        .is_some_and(|value| !value.is_null())
    {
        summary.orphan_workspaces += 1;
    }
    if instance
        .data
        .pointer("/pr_number")
        .is_some_and(|value| !value.is_null())
        || instance
            .data
            .pointer("/eval/pr_number")
            .is_some_and(|value| !value.is_null())
    {
        summary.orphan_pull_requests += 1;
    }

    for command in store.commands_for(workflow_id).await? {
        if active_command_status(command.status) {
            summary.active_commands += 1;
        }
        for job in store.runtime_jobs_for_command(&command.id).await? {
            if active_runtime_job_status(job.status) {
                summary.active_runtime_jobs += 1;
            }
        }
    }

    Ok(())
}

fn eval_cleanup_data(mut data: Value, eval_run_id: &str, case_id: &str, reason: &str) -> Value {
    if !data.is_object() {
        data = json!({});
    }
    let Some(object) = data.as_object_mut() else {
        return data;
    };
    let eval = object
        .entry("eval".to_string())
        .or_insert_with(|| json!({}));
    if !eval.is_object() {
        *eval = json!({});
    }
    if let Some(eval_object) = eval.as_object_mut() {
        eval_object.insert(
            "cleanup".to_string(),
            json!({
                "status": "cancelled",
                "eval_run_id": eval_run_id,
                "case_id": case_id,
                "reason": reason,
            }),
        );
    }
    data
}

fn active_command_status(status: WorkflowCommandStatus) -> bool {
    matches!(
        status,
        WorkflowCommandStatus::Pending
            | WorkflowCommandStatus::Dispatching
            | WorkflowCommandStatus::Dispatched
    )
}

fn active_runtime_job_status(status: RuntimeJobStatus) -> bool {
    matches!(
        status,
        RuntimeJobStatus::Pending | RuntimeJobStatus::Running
    )
}

fn eval_case_initial_instance(input: EvalCaseWorkflowInput<'_>) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "discovered",
        WorkflowSubject::new("issue", format!("issue:{}", input.case.issue)),
    )
    .with_id(eval_case_workflow_id(
        input.eval_run_id,
        &input.case.case_id,
    ))
    .with_data(eval_case_submitted_data(input, "created"))
}

fn eval_case_submitted_data(input: EvalCaseWorkflowInput<'_>, last_decision: &str) -> Value {
    json!({
        "project_id": input.project_id,
        "repo": input.case.repo,
        "issue_number": input.case.issue,
        "submission_id": input.task_id,
        "task_id": input.task_id,
        "task_ids": [input.task_id],
        "force_execute": true,
        "source": "eval_run",
        "external_id": input.case.case_id,
        "eval": {
            "eval_run_id": input.eval_run_id,
            "case_id": input.case.case_id,
            "base_commit": input.case.base_commit,
            "verify_commands": input.case.verify_commands,
            "timeout_secs": input.case.timeout_secs,
            "branch_prefix": EVAL_BRANCH_PREFIX,
            "pull_request_mode": EVAL_PR_DRAFT_MODE,
        },
        "last_decision": last_decision,
        "execution_path": "workflow_runtime",
    })
}

fn with_eval_command_metadata(
    mut decision: crate::runtime::WorkflowDecision,
    input: EvalCaseWorkflowInput<'_>,
) -> crate::runtime::WorkflowDecision {
    for command in &mut decision.commands {
        let Some(object) = command.command.as_object_mut() else {
            continue;
        };
        object.insert(
            "eval".to_string(),
            json!({
                "eval_run_id": input.eval_run_id,
                "case_id": input.case.case_id,
                "base_commit": input.case.base_commit,
                "verify_commands": input.case.verify_commands,
                "timeout_secs": input.case.timeout_secs,
                "branch_prefix": EVAL_BRANCH_PREFIX,
                "pull_request_mode": EVAL_PR_DRAFT_MODE,
            }),
        );
        object.insert("branch_prefix".to_string(), json!(EVAL_BRANCH_PREFIX));
        object.insert("pull_request_mode".to_string(), json!(EVAL_PR_DRAFT_MODE));
        object.insert("base_commit".to_string(), json!(input.case.base_commit));
        object.insert(
            "validation_commands".to_string(),
            json!(input.case.verify_commands),
        );
    }
    decision
}

fn eval_case_workflow_id(eval_run_id: &str, case_id: &str) -> String {
    format!("eval:{eval_run_id}:{case_id}")
}

const EVAL_CASE_DEFAULT_ADDITIONAL_PROMPT: &str = "\
This is a Harness eval run. Execute through the normal workflow runtime path, \
open only a draft pull request, use the harness-eval/ branch prefix, and do not \
merge or close the eval-produced PR.";

fn eval_case_additional_prompt(additional_prompt: Option<&str>) -> String {
    match additional_prompt
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        Some(prompt) => format!("{EVAL_CASE_DEFAULT_ADDITIONAL_PROMPT}\n\n{prompt}"),
        None => EVAL_CASE_DEFAULT_ADDITIONAL_PROMPT.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimeKind, RuntimeProfile, WorkflowRuntimeStore};

    #[test]
    fn eval_run_plan_marks_issue_submission_for_draft_prs() {
        let case = EvalBenchmarkCase {
            case_id: "owner/repo#42".to_string(),
            repo: "owner/repo".to_string(),
            issue: 42,
            base_commit: "abcdef1".to_string(),
            verify_commands: vec!["cargo test -p harness-workflow eval_run".to_string()],
            timeout_secs: 120,
        };
        let input = EvalCaseWorkflowInput {
            eval_run_id: "run-1",
            case: &case,
            project_id: "/repo",
            task_id: "eval-task-1",
            additional_prompt: None,
        };

        let initial = eval_case_initial_instance(input);
        assert_eq!(initial.id, "eval:run-1:owner/repo#42");
        assert_eq!(initial.definition_id, GITHUB_ISSUE_PR_DEFINITION_ID);
        assert_eq!(initial.data["eval"]["eval_run_id"], "run-1");
        assert_eq!(initial.data["eval"]["branch_prefix"], EVAL_BRANCH_PREFIX);
        assert_eq!(
            initial.data["eval"]["pull_request_mode"],
            EVAL_PR_DRAFT_MODE
        );

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
        let decision = with_eval_command_metadata(output.decision, input);
        let command = &decision.commands[0].command;
        assert_eq!(command["activity"], "implement_issue");
        assert_eq!(command["eval"]["eval_run_id"], "run-1");
        assert_eq!(command["branch_prefix"], EVAL_BRANCH_PREFIX);
        assert_eq!(command["pull_request_mode"], EVAL_PR_DRAFT_MODE);
        assert_eq!(
            command["validation_commands"][0],
            "cargo test -p harness-workflow eval_run"
        );
    }

    #[test]
    fn eval_run_prompt_preserves_required_draft_pr_constraints() {
        let prompt = eval_case_additional_prompt(Some("Use the small implementation slice."));
        assert!(prompt.contains("open only a draft pull request"));
        assert!(prompt.contains("harness-eval/ branch prefix"));
        assert!(prompt.contains("Use the small implementation slice."));
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
        if std::env::var_os("HARNESS_DATABASE_URL").is_none() {
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
            timeout_secs: 120,
        };
        let outcome = enqueue_eval_case_workflow(
            &store,
            EvalCaseWorkflowInput {
                eval_run_id: "run-cleanup",
                case: &case,
                project_id: dir.path().to_string_lossy().as_ref(),
                task_id: "eval-task-1",
                additional_prompt: None,
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
    async fn eval_run_dispatches_standard_runtime_job() -> anyhow::Result<()> {
        if std::env::var_os("HARNESS_DATABASE_URL").is_none() {
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
            timeout_secs: 120,
        };

        let outcome = dispatch_eval_case_workflow(
            &store,
            RuntimeProfile::new("codex", RuntimeKind::CodexExec),
            EvalCaseWorkflowInput {
                eval_run_id: "run-1",
                case: &case,
                project_id: dir.path().to_string_lossy().as_ref(),
                task_id: "eval-task-1",
                additional_prompt: None,
            },
        )
        .await?;

        assert_eq!(outcome.dispatched_jobs, 1);
        assert_eq!(outcome.enqueue.command_ids.len(), 1);
        let jobs = store
            .runtime_jobs_for_command(&outcome.enqueue.command_ids[0])
            .await?;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].input["activity"], "implement_issue");
        assert_eq!(jobs[0].input["command"]["eval"]["eval_run_id"], "run-1");
        assert_eq!(
            jobs[0].input["command"]["branch_prefix"],
            EVAL_BRANCH_PREFIX
        );
        assert_eq!(
            jobs[0].input["command"]["pull_request_mode"],
            EVAL_PR_DRAFT_MODE
        );

        Ok(())
    }
}
