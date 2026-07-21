use harness_core::config::isolation::IsolationTrustClass;
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
    author_trust_class: IsolationTrustClass,
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
        "author_trust_class": author_trust_class,
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

pub(super) fn preserve_recovery_metadata(
    data: &mut serde_json::Value,
    existing: &serde_json::Value,
) -> anyhow::Result<()> {
    for field in ["submission_id", "task_id", "task_ids"] {
        if let Some(value) = existing.get(field) {
            data[field] = value.clone();
        }
    }
    if let Some(value) = existing.get("author_trust_class") {
        let existing_trust: IsolationTrustClass = serde_json::from_value(value.clone())
            .map_err(|error| anyhow::anyhow!("invalid recovered author_trust_class: {error}"))?;
        let incoming_trust: IsolationTrustClass = serde_json::from_value(
            data.get("author_trust_class")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("recovery author_trust_class is missing"))?,
        )?;
        if existing_trust == IsolationTrustClass::NonCollaborator
            || incoming_trust == IsolationTrustClass::NonCollaborator
        {
            data["author_trust_class"] = json!(IsolationTrustClass::NonCollaborator);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_never_elevates_existing_non_collaborator() -> anyhow::Result<()> {
        let mut recovered = json!({"author_trust_class": "trusted"});
        preserve_recovery_metadata(
            &mut recovered,
            &json!({"author_trust_class": "non_collaborator"}),
        )?;
        assert_eq!(recovered["author_trust_class"], "non_collaborator");
        Ok(())
    }

    #[test]
    fn malformed_existing_trust_fails_closed() {
        let error = preserve_recovery_metadata(
            &mut json!({"author_trust_class": "trusted"}),
            &json!({"author_trust_class": "unknown"}),
        )
        .expect_err("invalid durable trust must fail");
        assert!(error
            .to_string()
            .contains("invalid recovered author_trust_class"));
    }
}
