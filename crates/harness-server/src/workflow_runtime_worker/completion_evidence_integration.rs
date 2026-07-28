//! Wires GH-1766 server-verified completion evidence into the runtime
//! worker's agent-turn path: strips forgeable reserved artifacts, re-executes
//! quality-gate validation commands server-side, verifies claimed PR
//! bindings, and records the operator kill switch as an explicit waiver
//! artifact.

use crate::http::AppState;
use harness_core::config::workflow::WorkflowConfig;
use harness_workflow::runtime::quality_gate::QUALITY_GATE_ACTIVITY;
use harness_workflow::runtime::{ActivityResult, ActivityStatus, RuntimeJob, WorkflowInstance};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::data_helpers::activity_name;
use super::pr_binding_verification::{attach_pr_binding_verification, result_claims_pr_binding};
use super::server_validation::{apply_server_validation, run_validation_commands};

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
        let commands = validation_commands_for_job(job, workflow);
        let run = run_validation_commands(
            workspace_root,
            &commands,
            Duration::from_secs(policy.quality_gate_validation_timeout_secs),
        )
        .await;
        result = apply_server_validation(result, run);
    }

    if result_claims_pr_binding(job, workflow, &result) {
        if let Some(workflow) = workflow {
            result = attach_pr_binding_verification(state, workflow, result).await;
        }
    }
    result
}

fn validation_commands_for_job(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Vec<String> {
    let from_value = |value: Option<&Value>| -> Option<Vec<String>> {
        let commands: Vec<String> = value?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
            .collect();
        if commands.is_empty() {
            None
        } else {
            Some(commands)
        }
    };
    from_value(job.input.get("validation_commands"))
        .or_else(|| {
            from_value(workflow.and_then(|workflow| workflow.data.get("validation_commands")))
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeKind, WorkflowSubject};

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
            "validation_commands": ["cargo test", " ", "cargo clippy"],
        }));
        assert_eq!(
            validation_commands_for_job(&job, None),
            vec!["cargo test".to_string(), "cargo clippy".to_string()]
        );

        let job = job_with_input(json!({ "activity": QUALITY_GATE_ACTIVITY }));
        let workflow = WorkflowInstance::new(
            "quality_gate",
            1,
            "checking",
            WorkflowSubject::new("quality_gate", "pr:1"),
        )
        .with_data(json!({ "validation_commands": ["cargo fmt --all -- --check"] }));
        assert_eq!(
            validation_commands_for_job(&job, Some(&workflow)),
            vec!["cargo fmt --all -- --check".to_string()]
        );
        assert!(validation_commands_for_job(&job, None).is_empty());
    }
}
