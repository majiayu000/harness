use super::manifest::EvalBenchmarkCase;
use crate::runtime::{
    build_issue_submission_decision, IssueSubmissionDecisionInput, RuntimeCommandDispatcher,
    RuntimeProfile, ValidationContext, WorkflowCommandStatus, WorkflowDecisionTransition,
    WorkflowDefinition, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID,
};
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
