use super::{
    data::eval_cleanup_data,
    manifest::{EvalBenchmarkCase, EvalIsolationProfile},
    transition_outcome::accepted_transition_record,
    trusted_verifier::{is_trusted_eval_verifier_argv, TRUSTED_EVAL_VERIFIER_V1_CAPABILITY},
};
use crate::runtime::{
    build_issue_submission_decision, github_issue_pr_definition_hash, IssueSubmissionDecisionInput,
    RuntimeJobStatus, RuntimeProfile, SubmissionMode, ValidationContext, WorkflowCommand,
    WorkflowCommandStatus, WorkflowCommandType, WorkflowDecision, WorkflowDecisionTransition,
    WorkflowDefinition, WorkflowEvidence, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    GITHUB_ISSUE_PR_DEFINITION_ID, GITHUB_ISSUE_PR_DEFINITION_VERSION,
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

#[derive(Debug, Clone, Copy)]
pub struct EvalCaseWorkflowInput<'a> {
    pub eval_run_id: &'a str,
    pub case: &'a EvalBenchmarkCase,
    pub project_id: &'a str,
    pub task_id: &'a str,
    pub additional_prompt: Option<&'a str>,
    pub timeout_secs: u64,
    pub resource_limits: &'a harness_sandbox::CappedResourceLimits,
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
    validate_eval_case_replayable(input.case)?;
    let verification_argv = input.case.verification_command_argv()?;

    store
        .upsert_definition(
            &WorkflowDefinition::new(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                GITHUB_ISSUE_PR_DEFINITION_VERSION,
                "GitHub issue PR workflow",
            )
            .with_definition_hash(github_issue_pr_definition_hash())
            .with_source_path(EVAL_RUN_DEFINITION_SOURCE),
        )
        .await?;

    let initial_instance = eval_case_initial_instance(input, &verification_argv);
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
    let decision = with_eval_command_metadata(output.decision, input, &verification_argv);
    let validator = crate::runtime::DecisionValidator::github_issue_pr();
    validator.validate(
        &initial_instance,
        &decision,
        &ValidationContext::new("eval-run", chrono::Utc::now()),
    )?;

    let mut submitted_instance = initial_instance.clone();
    submitted_instance.state = decision.next_state.clone();
    submitted_instance.version = submitted_instance.version.saturating_add(1);
    submitted_instance.replace_classified_data(
        eval_case_submitted_data(input, &decision.decision, &verification_argv),
        crate::runtime::DataProvenance::Server,
    );
    let record = accepted_transition_record(
        store
            .apply_decision_transition(
                WorkflowDecisionTransition {
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
                },
                "eval-run",
            )
            .await?,
        &initial_instance.id,
        "eval case enqueue",
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "eval case workflow {} changed or disappeared before commit",
            initial_instance.id
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

fn validate_eval_case_replayable(case: &EvalBenchmarkCase) -> anyhow::Result<()> {
    if let Some(blocker) = case.replay_blocker() {
        anyhow::bail!("eval case {} is not replayable: {blocker}", case.case_id);
    }
    case.verification_command_argv()?;
    Ok(())
}

pub fn eval_isolated_runtime_profile(
    case: &EvalBenchmarkCase,
    timeout_secs: u64,
) -> RuntimeProfile {
    let mut profile = RuntimeProfile::new(
        case.isolation.runtime_profile.clone(),
        case.isolation.runtime_kind,
    );
    profile.sandbox = Some(case.isolation.sandbox.clone());
    profile.timeout_secs = Some(timeout_secs);
    profile
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
                Ok(Some(_)) => summary.workflows_cancelled += 1,
                Ok(None) => {}
                Err(error) => {
                    summary.record_failure(&case.case_id, &workflow_id, "cancel_workflow", error);
                }
            }
            final_instance = match store.get_instance(&workflow_id).await {
                Ok(Some(latest_instance)) => latest_instance,
                Ok(None) => {
                    summary.record_failure(
                        &case.case_id,
                        &workflow_id,
                        "reload_workflow",
                        "workflow disappeared after cleanup transition",
                    );
                    instance.clone()
                }
                Err(error) => {
                    summary.record_failure(&case.case_id, &workflow_id, "reload_workflow", error);
                    continue;
                }
            };
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
    let cleanup_data = eval_cleanup_data(
        final_instance.data.clone(),
        eval_run_id,
        &case.case_id,
        reason,
    );
    final_instance.replace_classified_data(cleanup_data, crate::runtime::DataProvenance::Server);

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

    let record = accepted_transition_record(
        store
            .apply_decision_transition(
                WorkflowDecisionTransition {
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
                },
                "eval-cleanup",
            )
            .await?,
        &instance.id,
        "eval cleanup",
    )?;
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

    let commands = store.commands_for(workflow_id).await?;
    let mut jobs_by_command = super::cleanup::runtime_jobs_by_command_id(store, &commands).await?;
    for command in commands {
        if active_command_status(command.status) {
            summary.active_commands += 1;
        }
        for job in jobs_by_command.remove(&command.id).unwrap_or_default() {
            if active_runtime_job_status(job.status) {
                summary.active_runtime_jobs += 1;
            }
        }
    }

    Ok(())
}

fn active_command_status(status: WorkflowCommandStatus) -> bool {
    matches!(
        status,
        WorkflowCommandStatus::Pending
            | WorkflowCommandStatus::Dispatching
            | WorkflowCommandStatus::Deferred
            | WorkflowCommandStatus::Dispatched
    )
}

fn active_runtime_job_status(status: RuntimeJobStatus) -> bool {
    matches!(
        status,
        RuntimeJobStatus::Pending | RuntimeJobStatus::Running
    )
}

pub(super) fn eval_case_initial_instance(
    input: EvalCaseWorkflowInput<'_>,
    verification_argv: &[Vec<String>],
) -> WorkflowInstance {
    WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "discovered",
        WorkflowSubject::new("issue", format!("issue:{}", input.case.issue)),
    )
    .with_id(eval_case_workflow_id(
        input.eval_run_id,
        &input.case.case_id,
    ))
    .with_classified_data(
        eval_case_submitted_data(input, "created", verification_argv),
        crate::runtime::DataProvenance::Server,
    )
}

fn eval_case_submitted_data(
    input: EvalCaseWorkflowInput<'_>,
    last_decision: &str,
    verification_argv: &[Vec<String>],
) -> Value {
    json!({
        "definition_hash": github_issue_pr_definition_hash(),
        "project_id": input.project_id,
        "repo": input.case.repo,
        "issue_number": input.case.issue,
        "author_trust_class": "non_collaborator",
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
            "verify_commands_argv": verification_argv,
            "timeout_secs": input.timeout_secs,
            "resource_limits": input.resource_limits,
            "required_runtime_host_capabilities": eval_required_runtime_host_capabilities(verification_argv),
            "branch_prefix": EVAL_BRANCH_PREFIX,
            "pull_request_mode": EVAL_PR_DRAFT_MODE,
            "isolation": eval_isolation_metadata(&input.case.isolation),
        },
        "last_decision": last_decision,
        "execution_path": "workflow_runtime",
    })
}

fn with_eval_command_metadata(
    mut decision: crate::runtime::WorkflowDecision,
    input: EvalCaseWorkflowInput<'_>,
    verification_argv: &[Vec<String>],
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
                "verify_commands_argv": verification_argv,
                "timeout_secs": input.timeout_secs,
                "resource_limits": input.resource_limits,
                "required_runtime_host_capabilities": eval_required_runtime_host_capabilities(verification_argv),
                "branch_prefix": EVAL_BRANCH_PREFIX,
                "pull_request_mode": EVAL_PR_DRAFT_MODE,
                "isolation": eval_isolation_metadata(&input.case.isolation),
            }),
        );
        object.insert("branch_prefix".to_string(), json!(EVAL_BRANCH_PREFIX));
        object.insert("pull_request_mode".to_string(), json!(EVAL_PR_DRAFT_MODE));
        object.insert("base_commit".to_string(), json!(input.case.base_commit));
        object.insert(
            "validation_commands".to_string(),
            json!(input.case.verify_commands),
        );
        object.insert(
            "validation_commands_argv".to_string(),
            json!(verification_argv),
        );
    }
    decision
}

fn eval_isolation_metadata(isolation: &EvalIsolationProfile) -> Value {
    json!({
        "tier": isolation.tier,
        "runtime_kind": isolation.runtime_kind,
        "runtime_profile": isolation.runtime_profile,
        "sandbox": isolation.sandbox,
        "backend": isolation.backend,
        "image": isolation.image,
        "lifecycle": isolation.lifecycle,
        "cleanup_required": isolation.cleanup_required,
    })
}

fn eval_required_runtime_host_capabilities(verification_argv: &[Vec<String>]) -> Vec<&'static str> {
    let mut capabilities = vec![harness_sandbox::EVAL_RESOURCE_LIMITS_CAPABILITY];
    if verification_argv
        .iter()
        .any(|argv| is_trusted_eval_verifier_argv(argv))
    {
        capabilities.push(TRUSTED_EVAL_VERIFIER_V1_CAPABILITY);
    }
    capabilities
}

fn eval_case_workflow_id(eval_run_id: &str, case_id: &str) -> String {
    format!("eval:{eval_run_id}:{case_id}")
}

const EVAL_CASE_DEFAULT_ADDITIONAL_PROMPT: &str = "\
This is a Harness eval run. Execute through the normal workflow runtime path, \
open only a draft pull request, use the harness-eval/ branch prefix, use the \
recorded eval isolation profile, keep untrusted case execution out of the \
caller/server environment, retain backend/image/lifecycle/cleanup evidence, \
and do not merge or close the eval-produced PR. Before making changes, check out \
the exact requested base commit. The runtime host independently reports the \
observed checkout commit to the server.";

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
#[path = "run_tests.rs"]
mod tests;
