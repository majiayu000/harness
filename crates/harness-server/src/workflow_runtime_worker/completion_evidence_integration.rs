//! Wires GH-1766 server-verified completion evidence into the runtime
//! worker's agent-turn path: strips forgeable reserved artifacts, re-executes
//! quality-gate validation commands server-side, verifies claimed PR
//! bindings, and records the operator kill switch as an explicit waiver
//! artifact.

use crate::http::AppState;
use crate::reconciliation::{fetch_issue_state_with_token, GitHubState};
use harness_core::config::workflow::WorkflowConfig;
use harness_workflow::runtime::completion_evidence::ARTIFACT_VERIFIED_ISSUE_STATE;
use harness_workflow::runtime::quality_gate::QUALITY_GATE_ACTIVITY;
use harness_workflow::runtime::{
    activity_result_has_closed_issue_evidence, ActivityArtifact, ActivityErrorKind, ActivityResult,
    ActivityStatus, RuntimeJob, WorkflowInstance, GITHUB_ISSUE_PR_DEFINITION_ID,
};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::data_helpers::activity_name;
use super::pr_binding_verification::{attach_pr_binding_verification, result_claims_pr_binding};
use super::server_validation::{
    apply_server_validation, run_validation_commands, ValidationCommandSpec,
};

const ISSUE_STATE_VERIFICATION_ATTEMPTS: u32 = 3;
const ISSUE_STATE_RETRY_DELAY_MS: u64 = 500;

/// Apply the completion-evidence pipeline to an agent-turn activity result.
///
/// This runs unconditionally. The deployment-global kill switch
/// (`workflow.completion_evidence_enforced = false`) governs whether a
/// missing proof *blocks* a transition, not whether the server bothers to
/// look: it strips the declared requirements from the transition table at
/// startup, and the reducers read that table. Verification therefore keeps
/// producing evidence in the audit trail even during a kill-switch release,
/// which is what makes turning enforcement back on a safe, observable step.
pub(super) async fn apply_completion_evidence(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    config: &WorkflowConfig,
    workspace_root: &Path,
    result: ActivityResult,
) -> ActivityResult {
    let policy = &config.runtime_completion;
    let mut result = result;
    if activity_name(job) == QUALITY_GATE_ACTIVITY && result.status == ActivityStatus::Succeeded {
        let commands = match validation_commands_for_job(job, workflow) {
            Ok(commands) => commands,
            Err(error) => {
                return ActivityResult::failed(
                    QUALITY_GATE_ACTIVITY,
                    "Quality gate validation command configuration was invalid.",
                    error,
                )
                .with_error_kind(ActivityErrorKind::Configuration);
            }
        };
        let credential_environment =
            match crate::eval_credentials::eval_credential_environment_for_job(job) {
                Ok(environment) => environment,
                Err(error) => {
                    return ActivityResult::failed(
                        QUALITY_GATE_ACTIVITY,
                        "Quality gate eval credential environment was invalid.",
                        error.to_string(),
                    )
                    .with_error_kind(harness_workflow::runtime::ActivityErrorKind::Configuration);
                }
            };
        let run = run_validation_commands(
            workspace_root,
            &commands,
            Duration::from_secs(policy.quality_gate_validation_timeout_secs),
            credential_environment.as_ref(),
        )
        .await;
        result = apply_server_validation(result, run);
    }

    apply_external_completion_evidence(state, job, workflow, result).await
}

pub(super) async fn apply_external_completion_evidence(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    mut result: ActivityResult,
) -> ActivityResult {
    if result_claims_pr_binding(job, workflow, &result) {
        if let Some(workflow) = workflow {
            result = attach_pr_binding_verification(state, workflow, result).await;
        }
    }
    if result_claims_issue_closure(workflow, &result) {
        if let Some(workflow) = workflow {
            result = attach_issue_state_verification(
                state,
                workflow,
                result,
                state
                    .core
                    .server
                    .config
                    .workflow
                    .completion_evidence_enforced,
            )
            .await;
        }
    }
    result
}

fn result_claims_issue_closure(
    workflow: Option<&WorkflowInstance>,
    result: &ActivityResult,
) -> bool {
    workflow.is_some_and(|workflow| {
        workflow.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
            && activity_result_has_closed_issue_evidence(result)
    })
}

async fn attach_issue_state_verification(
    state: &Arc<AppState>,
    workflow: &WorkflowInstance,
    result: ActivityResult,
    evidence_enforced: bool,
) -> ActivityResult {
    let Some((repo, issue_number)) = workflow_issue_target(workflow) else {
        return result;
    };
    let token = state.core.server.config.server.github_token.as_deref();
    for attempt in 0..ISSUE_STATE_VERIFICATION_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(
                ISSUE_STATE_RETRY_DELAY_MS * u64::from(attempt),
            ))
            .await;
        }
        match fetch_issue_state_with_token(&repo, issue_number, token).await {
            GitHubState::IssueClosed | GitHubState::IssueCompleted => {
                return result.with_artifact(ActivityArtifact::new(
                    ARTIFACT_VERIFIED_ISSUE_STATE,
                    serde_json::json!({
                        "issue_number": issue_number,
                        "repo": repo,
                        "state": "closed",
                        "issue_url": format!("https://github.com/{repo}/issues/{issue_number}"),
                        "snapshot_source": "server_github_rest",
                    }),
                ));
            }
            GitHubState::Open | GitHubState::PrMerged | GitHubState::PrClosed => return result,
            GitHubState::Unknown => {}
        }
    }

    issue_verification_unavailable(result, &repo, issue_number, evidence_enforced)
}

fn issue_verification_unavailable(
    result: ActivityResult,
    repo: &str,
    issue_number: u64,
    evidence_enforced: bool,
) -> ActivityResult {
    if !evidence_enforced {
        return result;
    }
    let mut failed = result;
    failed.summary = format!(
        "Server could not verify the reported closed issue {repo}#{issue_number} after \
         {ISSUE_STATE_VERIFICATION_ATTEMPTS} attempts."
    );
    failed.error = Some("issue state verification transport failure".to_string());
    failed.error_kind = Some(ActivityErrorKind::ExternalDependency);
    failed.status = ActivityStatus::Failed;
    failed
}

fn workflow_issue_target(workflow: &WorkflowInstance) -> Option<(String, u64)> {
    let repo = workflow
        .data
        .get("repo")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let issue_number = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64())
        .or_else(|| workflow.subject.subject_key.parse().ok())?;
    Some((repo, issue_number))
}

fn validation_commands_for_job(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<Vec<ValidationCommandSpec>, String> {
    for value in [
        job.input.get("validation_commands_argv"),
        workflow.and_then(|workflow| workflow.data.get("validation_commands_argv")),
    ]
    .into_iter()
    .flatten()
    {
        let commands = validation_command_argv_from_value(value)?;
        if !commands.is_empty() {
            return Ok(commands);
        }
    }
    for value in [
        job.input.get("validation_commands"),
        workflow.and_then(|workflow| workflow.data.get("validation_commands")),
    ]
    .into_iter()
    .flatten()
    {
        let commands = validation_command_strings_from_value(value)?;
        if !commands.is_empty() {
            return Ok(commands);
        }
    }
    Ok(Vec::new())
}

fn validation_command_argv_from_value(value: &Value) -> Result<Vec<ValidationCommandSpec>, String> {
    let commands = value
        .as_array()
        .ok_or_else(|| "validation_commands_argv must be an array".to_string())?;
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let arguments = command
                .as_array()
                .ok_or_else(|| format!("validation_commands_argv[{index}] must be an array"))?;
            let argv = arguments
                .iter()
                .enumerate()
                .map(|(argument_index, argument)| {
                    argument.as_str().map(str::to_string).ok_or_else(|| {
                        format!(
                            "validation_commands_argv[{index}][{argument_index}] must be a string"
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            ValidationCommandSpec::from_argv(argv)
                .map_err(|error| format!("validation_commands_argv[{index}]: {error}"))
        })
        .collect()
}

fn validation_command_strings_from_value(
    value: &Value,
) -> Result<Vec<ValidationCommandSpec>, String> {
    let commands = value
        .as_array()
        .ok_or_else(|| "validation_commands must be an array".to_string())?;
    commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let command = command
                .as_str()
                .ok_or_else(|| format!("validation_commands[{index}] must be a string"))?;
            ValidationCommandSpec::from_legacy_string(command)
                .map_err(|error| format!("validation_commands[{index}]: {error}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{ActivitySignal, RuntimeKind, WorkflowSubject};

    fn job_with_input(input: Value) -> RuntimeJob {
        let mut job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": QUALITY_GATE_ACTIVITY }),
        );
        job.input = input;
        job
    }

    #[test]
    fn validation_commands_prefer_job_input_then_workflow_data() {
        let job = job_with_input(json!({
            "activity": QUALITY_GATE_ACTIVITY,
            "validation_commands": ["cargo test", "cargo clippy"],
        }));
        assert_eq!(
            validation_commands_for_job(&job, None).unwrap(),
            vec![
                ValidationCommandSpec::from_legacy_string("cargo test").unwrap(),
                ValidationCommandSpec::from_legacy_string("cargo clippy").unwrap(),
            ]
        );

        let job = job_with_input(json!({ "activity": QUALITY_GATE_ACTIVITY }));
        let workflow = WorkflowInstance::new(
            "quality_gate",
            1,
            "checking",
            WorkflowSubject::new("quality_gate", "pr:1"),
        )
        .with_server_data(json!({ "validation_commands": ["cargo fmt --all -- --check"] }));
        assert_eq!(
            validation_commands_for_job(&job, Some(&workflow)).unwrap(),
            vec![ValidationCommandSpec::from_legacy_string("cargo fmt --all -- --check").unwrap()]
        );
        assert!(validation_commands_for_job(&job, None).unwrap().is_empty());
    }

    #[test]
    fn validation_commands_prefer_argv_arrays_and_preserve_argument_boundaries() {
        let job = job_with_input(json!({
            "activity": QUALITY_GATE_ACTIVITY,
            "validation_commands_argv": [["cargo", "test", "name with spaces"]],
            "validation_commands": ["cargo test should-not-run"],
        }));

        assert_eq!(
            validation_commands_for_job(&job, None).unwrap(),
            vec![ValidationCommandSpec::from_argv(vec![
                "cargo".to_string(),
                "test".to_string(),
                "name with spaces".to_string(),
            ])
            .unwrap()]
        );
    }

    #[test]
    fn validation_command_argv_preserves_legal_empty_arguments() {
        let job = job_with_input(json!({
            "validation_commands_argv": [["tool", ""]],
        }));

        assert_eq!(
            validation_commands_for_job(&job, None).unwrap(),
            vec![
                ValidationCommandSpec::from_argv(vec!["tool".to_string(), "".to_string(),])
                    .unwrap()
            ]
        );
    }

    #[test]
    fn validation_commands_fail_closed_when_any_entry_is_malformed() {
        let job = job_with_input(json!({
            "validation_commands_argv": [["cargo", "test"], ["", "ignored"]],
        }));

        let error = validation_commands_for_job(&job, None)
            .expect_err("one malformed command must reject the entire validation set");
        assert!(error.contains("validation_commands_argv[1]"));
        assert!(error.contains("program must not be empty"));
    }

    #[test]
    fn empty_structured_commands_fall_back_to_legacy_strings() {
        let job = job_with_input(json!({
            "validation_commands_argv": [],
            "validation_commands": ["cargo test"],
        }));

        assert_eq!(
            validation_commands_for_job(&job, None).unwrap(),
            vec![ValidationCommandSpec::from_legacy_string("cargo test").unwrap()]
        );
    }

    #[test]
    fn issue_closure_claim_and_target_use_structured_server_identity() {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "123"),
        )
        .with_server_data(json!({ "repo": "owner/repo", "issue_number": 123 }));
        let result = ActivityResult::succeeded("implement_issue", "closed").with_signal(
            ActivitySignal::new(
                "IssueClosed",
                json!({ "issue_number": 123, "state": "closed" }),
            ),
        );
        assert!(result_claims_issue_closure(Some(&workflow), &result));
        assert_eq!(
            workflow_issue_target(&workflow),
            Some(("owner/repo".to_string(), 123))
        );
        let missing_target = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "123"),
        );
        assert_eq!(workflow_issue_target(&missing_target), None);
    }

    #[test]
    fn issue_verification_outage_honors_evidence_kill_switch() {
        let result = ActivityResult::succeeded("implement_issue", "closed");
        let waived = issue_verification_unavailable(result.clone(), "owner/repo", 123, false);
        assert_eq!(waived.status, ActivityStatus::Succeeded);
        let enforced = issue_verification_unavailable(result, "owner/repo", 123, true);
        assert_eq!(enforced.status, ActivityStatus::Failed);
        assert_eq!(
            enforced.error_kind,
            Some(ActivityErrorKind::ExternalDependency)
        );
    }
}
