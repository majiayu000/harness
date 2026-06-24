use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot, github_pr_snapshot_failure_result, pr_readiness_for_snapshot,
    GitHubPrSnapshotArtifacts, GitHubPrSnapshotTarget,
};
use crate::http::AppState;
use harness_workflow::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeJob, WorkflowInstance, PR_FEEDBACK_INSPECT_ACTIVITY,
};
use serde_json::Value;
use std::sync::Arc;

use super::data_helpers::{
    activity_name, optional_data_string, optional_data_u64, parse_pr_subject_key,
};

pub(super) fn is_server_owned_pr_feedback_inspection(job: &RuntimeJob) -> bool {
    activity_name(job) == PR_FEEDBACK_INSPECT_ACTIVITY
}

pub(super) async fn execute_pr_feedback_inspection(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> ActivityResult {
    let activity = activity_name(job);
    let target = match pr_snapshot_target(job, workflow) {
        Ok(target) => target,
        Err(error) => {
            return ActivityResult::failed(
                activity,
                "Server-owned PR feedback inspection target is incomplete.",
                error.to_string(),
            )
            .with_error_kind(ActivityErrorKind::Configuration)
        }
    };
    match fetch_github_pr_snapshot(
        &target,
        state.core.server.config.server.github_token.as_deref(),
    )
    .await
    {
        Ok(artifacts) => {
            match persist_pr_remote_fact_snapshot(state, &activity, &artifacts).await {
                Ok(()) => {
                    let readiness = pr_readiness_for_snapshot(&artifacts.normalized_snapshot);
                    tracing::info!(
                        repo = %target.repo_slug,
                        pr = target.pr_number,
                        readiness = readiness.as_str(),
                        "server-owned PR feedback inspection collected remote facts"
                    );
                    artifacts.activity_result(&activity)
                }
                Err(error) => ActivityResult::failed(
                    activity,
                    "Server-owned PR snapshot persistence failed.",
                    error.to_string(),
                )
                .with_error_kind(ActivityErrorKind::Fatal),
            }
        }
        Err(error) => github_pr_snapshot_failure_result(&activity, Some(&target), &error),
    }
}

async fn persist_pr_remote_fact_snapshot(
    state: &Arc<AppState>,
    activity: &str,
    artifacts: &GitHubPrSnapshotArtifacts,
) -> anyhow::Result<()> {
    let store = state.core.workflow_runtime_store.as_ref().ok_or_else(|| {
        anyhow::anyhow!("{activity} requires workflow_runtime_store to persist PR remote facts")
    })?;
    let snapshot = artifacts.remote_fact_snapshot()?;
    store.upsert_remote_fact_snapshot(&snapshot).await?;
    Ok(())
}

fn pr_snapshot_target(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> anyhow::Result<GitHubPrSnapshotTarget> {
    let pr_url = workflow
        .and_then(|workflow| optional_data_string(workflow, "pr_url"))
        .or_else(|| job_input_string(job, "pr_url"));
    let repo = workflow
        .and_then(|workflow| optional_data_string(workflow, "repo"))
        .or_else(|| job_input_string(job, "repo"))
        .or_else(|| pr_url.as_deref().and_then(repo_slug_from_url));
    let pr_number = workflow
        .and_then(|workflow| optional_data_u64(workflow, "pr_number"))
        .or_else(|| {
            workflow.and_then(|workflow| parse_pr_subject_key(&workflow.subject.subject_key))
        })
        .or_else(|| job_input_u64(job, "pr_number"))
        .or_else(|| pr_url.as_deref().and_then(pr_number_from_url));

    let repo = repo.ok_or_else(|| anyhow::anyhow!("GitHub repo slug is missing"))?;
    let pr_number = pr_number.ok_or_else(|| anyhow::anyhow!("PR number is missing"))?;
    let mut target = GitHubPrSnapshotTarget::new(repo, pr_number)?;
    if let Some(expected_base_ref) = expected_base_ref(job, workflow) {
        target = target.with_expected_base_ref(expected_base_ref);
    }
    Ok(target)
}

fn expected_base_ref(job: &RuntimeJob, workflow: Option<&WorkflowInstance>) -> Option<String> {
    workflow
        .and_then(|workflow| optional_data_string(workflow, "expected_base_ref"))
        .or_else(|| workflow.and_then(|workflow| optional_data_string(workflow, "target_base_ref")))
        .or_else(|| workflow.and_then(|workflow| optional_data_string(workflow, "base_ref")))
        .or_else(|| job_input_string(job, "expected_base_ref"))
        .or_else(|| job_input_string(job, "target_base_ref"))
        .or_else(|| job_input_string(job, "base_ref"))
}

fn job_input_string(job: &RuntimeJob, field: &str) -> Option<String> {
    json_string_field(&job.input, field).or_else(|| {
        job.input
            .get("command")
            .and_then(|command| json_string_field(command, field))
    })
}

fn job_input_u64(job: &RuntimeJob, field: &str) -> Option<u64> {
    json_u64_field(&job.input, field).or_else(|| {
        job.input
            .get("command")
            .and_then(|command| json_u64_field(command, field))
    })
}

fn json_string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn json_u64_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
    })
}

fn repo_slug_from_url(pr_url: &str) -> Option<String> {
    harness_core::prompts::parse_github_pr_url(pr_url)
        .map(|(owner, repo, _)| format!("{owner}/{repo}"))
}

fn pr_number_from_url(pr_url: &str) -> Option<u64> {
    harness_core::prompts::parse_github_pr_url(pr_url).map(|(_, _, number)| number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{RuntimeKind, WorkflowSubject};
    use serde_json::json;

    fn pr_feedback_runtime_job(input: Value) -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            input,
        )
    }

    #[test]
    fn pr_snapshot_target_prefers_workflow_data() {
        let job = pr_feedback_runtime_job(json!({
            "activity": PR_FEEDBACK_INSPECT_ACTIVITY,
            "repo": "wrong/repo",
            "pr_number": 88
        }));
        let workflow = WorkflowInstance::new(
            "pr_feedback",
            1,
            "inspecting",
            WorkflowSubject::new("pr", "pr:77"),
        )
        .with_data(json!({
            "repo": "owner/repo",
            "pr_number": 77,
            "pr_url": "https://github.com/owner/repo/pull/77"
        }));

        let target = pr_snapshot_target(&job, Some(&workflow)).unwrap();

        assert_eq!(target.repo_slug, "owner/repo");
        assert_eq!(target.pr_number, 77);
        assert_eq!(target.expected_base_ref, None);
    }

    #[test]
    fn pr_snapshot_target_can_parse_pr_url() {
        let job = pr_feedback_runtime_job(json!({
            "activity": PR_FEEDBACK_INSPECT_ACTIVITY,
            "pr_url": "https://github.com/owner/repo/pull/77"
        }));

        let target = pr_snapshot_target(&job, None).unwrap();

        assert_eq!(target.repo_slug, "owner/repo");
        assert_eq!(target.pr_number, 77);
        assert_eq!(target.expected_base_ref, None);
    }

    #[test]
    fn pr_snapshot_target_preserves_explicit_expected_base_ref() {
        let job = pr_feedback_runtime_job(json!({
            "activity": PR_FEEDBACK_INSPECT_ACTIVITY,
            "repo": "owner/repo",
            "pr_number": 77,
            "expected_base_ref": "release"
        }));

        let target = pr_snapshot_target(&job, None).unwrap();

        assert_eq!(target.expected_base_ref.as_deref(), Some("release"));
    }

    #[test]
    fn pr_snapshot_target_reports_missing_context() {
        let job = pr_feedback_runtime_job(json!({"activity": PR_FEEDBACK_INSPECT_ACTIVITY}));

        let error = pr_snapshot_target(&job, None).unwrap_err();

        assert!(error.to_string().contains("GitHub repo slug is missing"));
    }

    #[test]
    fn inspect_pr_feedback_is_server_owned() {
        let inspect = pr_feedback_runtime_job(json!({"activity": PR_FEEDBACK_INSPECT_ACTIVITY}));
        let implement = pr_feedback_runtime_job(json!({"activity": "implement_issue"}));

        assert!(is_server_owned_pr_feedback_inspection(&inspect));
        assert!(!is_server_owned_pr_feedback_inspection(&implement));
    }
}
