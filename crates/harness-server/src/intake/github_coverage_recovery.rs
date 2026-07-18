use harness_workflow::runtime::WorkflowRuntimeStore;
use serde_json::json;
use std::path::Path;

use crate::workflow_runtime_pr_feedback::pr_detection::ClosingPullRequestCandidate;

#[allow(clippy::too_many_arguments)]
pub(super) fn recovered_workflow_data(
    project_root: &Path,
    project_id: &str,
    repo: &str,
    issue_number: u64,
    candidate: &ClosingPullRequestCandidate,
    snapshot: &serde_json::Value,
    fact_hash: &str,
    state: &str,
) -> serde_json::Value {
    let task_id = format!("github-issue:{repo}:issue:{issue_number}");
    let mut data = json!({
        "project_id": project_id,
        "repo": repo,
        "source": "github",
        "submission_id": task_id,
        "task_id": task_id,
        "task_ids": [task_id],
        "issue_number": issue_number,
        "pr_number": candidate.number,
        "pr_url": candidate.url,
        "pr_head_ref": candidate.head_ref_name,
        "pr_head_sha": snapshot.get("head_oid").cloned().unwrap_or(serde_json::Value::Null),
        "expected_base_ref": snapshot.get("expected_base_ref").cloned().unwrap_or(serde_json::Value::Null),
        "last_remote_fact_hash": fact_hash,
        "coverage_recovered_from_github": true,
        "recovered_pr_snapshot": snapshot,
    });
    if state == "done" {
        data["terminal_evidence"] = json!({
            "source": "server_github_graphql",
            "reason": "closing_pr_merged",
            "fact_hash": fact_hash,
            "merge_commit_sha": snapshot
                .get("merge_commit_sha")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        });
    }
    crate::workflow_runtime_policy::merge_runtime_retry_policy(project_root, data)
}

pub(super) fn preserve_submission_handles(
    data: &mut serde_json::Value,
    existing: &serde_json::Value,
) {
    for field in ["submission_id", "task_id", "task_ids"] {
        if let Some(value) = existing.get(field) {
            data[field] = value.clone();
        }
    }
}

pub(super) async fn cancel_superseded_commands(
    runtime_store: &WorkflowRuntimeStore,
    workflow_id: &str,
) -> anyhow::Result<()> {
    for command in runtime_store.commands_for(workflow_id).await? {
        runtime_store
            .cancel_command_and_unfinished_runtime_jobs(
                &command.id,
                "github_coverage_recovery",
                "GitHub reported an existing closing pull request.",
            )
            .await?;
    }
    Ok(())
}
