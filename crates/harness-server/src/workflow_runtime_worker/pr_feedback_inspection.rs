use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot, github_pr_snapshot_failure_result, GitHubPrSnapshotTarget,
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
        Ok(artifacts) => artifacts.activity_result(&activity),
        Err(error) => github_pr_snapshot_failure_result(&activity, Some(&target), &error),
    }
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
    GitHubPrSnapshotTarget::new(repo, pr_number)
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
