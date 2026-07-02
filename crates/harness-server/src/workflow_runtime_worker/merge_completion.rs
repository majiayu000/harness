use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot, value_string, value_u64, GitHubPrSnapshotArtifacts,
    GitHubPrSnapshotTarget, GITHUB_PR_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ERROR_ARTIFACT,
};
use crate::http::AppState;
use harness_core::config::intake::GitHubAutoMergeConfig;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus, RuntimeJob,
    WorkflowInstance, GITHUB_ISSUE_PR_DEFINITION_ID, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use serde_json::{json, Value};

use super::data_helpers::activity_name;

const MERGE_COMPLETION_VERIFICATION_ARTIFACT: &str = "merge_completion_verification";
const MERGE_COMPLETION_VERIFICATION_SCHEMA: &str =
    "harness.github.merge_completion_verification.v1";
pub(super) async fn verify_merge_completion_if_needed(
    state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    result: ActivityResult,
) -> ActivityResult {
    if !merge_completion_needs_verification(job, workflow, &result) {
        return result;
    }
    let config = auto_merge_config(state);
    if !config.verify_merge_completion {
        return result;
    }
    let target = match merge_completion_target(job, workflow, &result) {
        Ok(target) => target,
        Err(error) => {
            return merge_completion_failed(
                result,
                ActivityErrorKind::Configuration,
                "Server-side merge completion verification failed.",
                &error,
                None,
            );
        }
    };
    match fetch_github_pr_snapshot(
        &target,
        state.core.server.config.server.github_token.as_deref(),
    )
    .await
    {
        Ok(snapshot) if snapshot_observes_merged(&snapshot.normalized_snapshot) => {
            merge_completion_verified(result, &target, snapshot)
        }
        Ok(snapshot) => merge_completion_failed(
            result,
            ActivityErrorKind::Fatal,
            "Server-side merge completion verification rejected agent output.",
            &format!(
                "agent reported merged=true for PR #{} in {}, but GitHub state was {}",
                target.pr_number,
                target.repo_slug,
                value_string(snapshot.normalized_snapshot.get("state"))
                    .unwrap_or_else(|| "<missing>".to_string())
            ),
            Some(snapshot),
        ),
        Err(error) => merge_completion_fetch_failed(result, &target, &error),
    }
}

pub(super) fn auto_merge_config(state: &AppState) -> GitHubAutoMergeConfig {
    state
        .core
        .server
        .config
        .intake
        .github
        .as_ref()
        .map(|config| config.auto_merge.clone())
        .unwrap_or_default()
}

fn merge_completion_needs_verification(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    result: &ActivityResult,
) -> bool {
    merge_activity_matches(job, workflow)
        && result.status == ActivityStatus::Succeeded
        && result_reports_merged(result)
}

pub(super) fn merge_activity_matches(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> bool {
    activity_name(job) == "merge_pr"
        && workflow
            .map(|workflow| workflow.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID)
            .unwrap_or(false)
}

fn merge_completion_target(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    result: &ActivityResult,
) -> Result<GitHubPrSnapshotTarget, String> {
    merge_activity_target(
        job,
        workflow,
        merged_pull_request_artifact(result)
            .and_then(|artifact| value_u64(artifact.get("pr_number"))),
    )
}

pub(super) fn merge_execution_target(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<GitHubPrSnapshotTarget, String> {
    merge_activity_target(job, workflow, None)
}

fn merge_activity_target(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    reported_pr_number: Option<u64>,
) -> Result<GitHubPrSnapshotTarget, String> {
    let workflow_data = workflow.map(|workflow| &workflow.data);
    let repo_slug = workflow_data
        .and_then(|data| value_string(data.get("repo")))
        .or_else(|| value_string(job.input.get("repo")))
        .ok_or_else(|| "merge_pr verification requires a workflow repo slug".to_string())?;
    let bound_pr_number = workflow_data
        .and_then(|data| value_u64(data.get("pr_number")))
        .or_else(|| value_u64(job.input.get("pr_number")));
    if let (Some(bound), Some(reported)) = (bound_pr_number, reported_pr_number) {
        if bound != reported {
            return Err(format!(
                "agent reported merged PR #{reported}, but workflow is bound to PR #{bound}"
            ));
        }
    }
    let pr_number = bound_pr_number
        .or(reported_pr_number)
        .ok_or_else(|| "merge_pr verification requires a PR number".to_string())?;
    let mut target =
        GitHubPrSnapshotTarget::new(repo_slug, pr_number).map_err(|error| error.to_string())?;
    if let Some(expected_base_ref) =
        workflow_data.and_then(crate::http::auto_merge::expected_base_ref_from_workflow_data)
    {
        target = target.with_expected_base_ref(expected_base_ref);
    }
    Ok(target)
}

fn result_reports_merged(result: &ActivityResult) -> bool {
    merged_pull_request_artifact(result).is_some()
}

fn merged_pull_request_artifact(result: &ActivityResult) -> Option<&Value> {
    result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "pull_request")
        .map(|artifact| &artifact.artifact)
        .find(|artifact| pull_request_artifact_is_merged(artifact))
}

fn pull_request_artifact_is_merged(value: &Value) -> bool {
    value.get("merged").and_then(Value::as_bool) == Some(true)
        || value
            .get("state")
            .and_then(|value| value_string(Some(value)))
            .is_some_and(|state| state.eq_ignore_ascii_case("merged"))
}

pub(super) fn snapshot_observes_merged(snapshot: &Value) -> bool {
    snapshot.get("merged").and_then(Value::as_bool) == Some(true)
        || snapshot
            .get("state")
            .and_then(|value| value_string(Some(value)))
            .is_some_and(|state| state.eq_ignore_ascii_case("MERGED"))
}

pub(super) fn merge_completion_verified(
    mut result: ActivityResult,
    target: &GitHubPrSnapshotTarget,
    snapshot: GitHubPrSnapshotArtifacts,
) -> ActivityResult {
    enrich_pull_request_artifact(&mut result, target, &snapshot.normalized_snapshot);
    append_verification_artifacts(&mut result, target, &snapshot, true, "verified");
    result
}

fn enrich_pull_request_artifact(
    result: &mut ActivityResult,
    target: &GitHubPrSnapshotTarget,
    snapshot: &Value,
) {
    for artifact in result
        .artifacts
        .iter_mut()
        .filter(|artifact| artifact.artifact_type == "pull_request")
    {
        if value_u64(artifact.artifact.get("pr_number")) != Some(target.pr_number) {
            continue;
        }
        let Some(object) = artifact.artifact.as_object_mut() else {
            continue;
        };
        object.insert("merged".to_string(), json!(true));
        object.insert(
            "state".to_string(),
            snapshot
                .get("state")
                .cloned()
                .unwrap_or_else(|| json!("MERGED")),
        );
        object.insert("server_verified".to_string(), json!(true));
        object.insert(
            "verification_source".to_string(),
            json!("server_github_graphql"),
        );
        copy_snapshot_field(object, snapshot, "observed_at");
        copy_snapshot_field_as(object, snapshot, "head_oid", "head_sha");
        copy_snapshot_field_as(object, snapshot, "merge_commit_sha", "merge_commit_sha");
        object.insert(
            "server_merge_verification".to_string(),
            verification_payload(target, snapshot, true, "verified"),
        );
    }
}

fn copy_snapshot_field(object: &mut serde_json::Map<String, Value>, snapshot: &Value, field: &str) {
    if let Some(value) = snapshot.get(field) {
        object.insert(field.to_string(), value.clone());
    }
}

fn copy_snapshot_field_as(
    object: &mut serde_json::Map<String, Value>,
    snapshot: &Value,
    source: &str,
    target: &str,
) {
    if let Some(value) = snapshot.get(source) {
        object.insert(target.to_string(), value.clone());
    }
}

fn merge_completion_failed(
    mut result: ActivityResult,
    error_kind: ActivityErrorKind,
    summary: &str,
    error: &str,
    snapshot: Option<GitHubPrSnapshotArtifacts>,
) -> ActivityResult {
    let target = snapshot.as_ref().and_then(|snapshot| {
        let repo = value_string(snapshot.normalized_snapshot.get("repo"))?;
        let pr_number = value_u64(snapshot.normalized_snapshot.get("pr_number"))?;
        GitHubPrSnapshotTarget::new(repo, pr_number).ok()
    });
    result.status = ActivityStatus::Failed;
    result.summary = summary.to_string();
    result.error = Some(error.to_string());
    result.error_kind = Some(error_kind);
    if let (Some(target), Some(snapshot)) = (target.as_ref(), snapshot.as_ref()) {
        append_verification_artifacts(&mut result, target, snapshot, false, "rejected");
    } else {
        result.artifacts.push(ActivityArtifact::new(
            MERGE_COMPLETION_VERIFICATION_ARTIFACT,
            json!({
                "schema": MERGE_COMPLETION_VERIFICATION_SCHEMA,
                "verified": false,
                "outcome": "rejected",
                "reason": error,
            }),
        ));
    }
    result
}

fn merge_completion_fetch_failed(
    mut result: ActivityResult,
    target: &GitHubPrSnapshotTarget,
    error: &anyhow::Error,
) -> ActivityResult {
    result.status = ActivityStatus::Failed;
    result.summary = "Server-side merge completion verification could not read GitHub.".to_string();
    result.error = Some(error.to_string());
    result.error_kind = Some(ActivityErrorKind::ExternalDependency);
    result.artifacts.push(ActivityArtifact::new(
        MERGE_COMPLETION_VERIFICATION_ARTIFACT,
        json!({
            "schema": MERGE_COMPLETION_VERIFICATION_SCHEMA,
            "verified": false,
            "outcome": "fetch_failed",
            "repo": target.repo_slug,
            "pr_number": target.pr_number,
            "reason": error.to_string(),
        }),
    ));
    result.artifacts.push(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ERROR_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot_error.v1",
            "repo": target.repo_slug,
            "pr_number": target.pr_number,
            "error": error.to_string(),
        }),
    ));
    result
}

fn append_verification_artifacts(
    result: &mut ActivityResult,
    target: &GitHubPrSnapshotTarget,
    snapshot: &GitHubPrSnapshotArtifacts,
    verified: bool,
    outcome: &str,
) {
    result.artifacts.push(ActivityArtifact::new(
        MERGE_COMPLETION_VERIFICATION_ARTIFACT,
        verification_payload(target, &snapshot.normalized_snapshot, verified, outcome),
    ));
    result.artifacts.push(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ARTIFACT,
        snapshot.normalized_snapshot.clone(),
    ));
    result.artifacts.push(ActivityArtifact::new(
        GITHUB_PR_SNAPSHOT_ARTIFACT,
        snapshot.raw_pr.clone(),
    ));
}

fn verification_payload(
    target: &GitHubPrSnapshotTarget,
    snapshot: &Value,
    verified: bool,
    outcome: &str,
) -> Value {
    json!({
        "schema": MERGE_COMPLETION_VERIFICATION_SCHEMA,
        "verified": verified,
        "outcome": outcome,
        "verification_source": "server_github_graphql",
        "repo": target.repo_slug,
        "pr_number": target.pr_number,
        "observed_merged": snapshot_observes_merged(snapshot),
        "state": snapshot.get("state").cloned().unwrap_or(Value::Null),
        "merged": snapshot.get("merged").cloned().unwrap_or(Value::Null),
        "observed_at": snapshot.get("observed_at").cloned().unwrap_or(Value::Null),
        "head_sha": snapshot.get("head_oid").cloned().unwrap_or(Value::Null),
        "merge_commit_sha": snapshot.get("merge_commit_sha").cloned().unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target() -> GitHubPrSnapshotTarget {
        GitHubPrSnapshotTarget::new("owner/repo", 77).expect("valid target")
    }

    fn activity_result() -> ActivityResult {
        ActivityResult::succeeded("merge_pr", "Agent says merged.").with_artifact(
            ActivityArtifact::new(
                "pull_request",
                json!({
                    "pr_number": 77,
                    "pr_url": "https://github.com/owner/repo/pull/77",
                    "merged": true,
                    "head_sha": "agent-head",
                }),
            ),
        )
    }

    fn snapshot(state: &str, merged: bool) -> GitHubPrSnapshotArtifacts {
        let normalized_snapshot = json!({
            "schema": "harness.github.pr_snapshot.v1",
            "snapshot_source": "server_github_graphql",
            "repo": "owner/repo",
            "pr_number": 77,
            "state": state,
            "merged": merged,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "observed_at": "2026-07-02T10:00:00Z",
            "head_oid": "server-head",
            "merge_commit_sha": if merged { json!("merge-sha") } else { Value::Null },
        });
        GitHubPrSnapshotArtifacts {
            raw_pr: normalized_snapshot.clone(),
            normalized_snapshot,
        }
    }

    #[test]
    fn verified_merge_enriches_pull_request_with_server_evidence() {
        let result =
            merge_completion_verified(activity_result(), &target(), snapshot("MERGED", true));

        assert_eq!(result.status, ActivityStatus::Succeeded);
        let pull_request = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "pull_request")
            .expect("pull request artifact");
        assert_eq!(pull_request.artifact["server_verified"], true);
        assert_eq!(pull_request.artifact["head_sha"], "server-head");
        assert_eq!(pull_request.artifact["merge_commit_sha"], "merge-sha");
        assert_eq!(
            pull_request.artifact["server_merge_verification"]["verified"],
            true
        );
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == MERGE_COMPLETION_VERIFICATION_ARTIFACT
                && artifact.artifact["verified"] == true
        }));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == SERVER_PR_SNAPSHOT_ARTIFACT));
    }

    #[test]
    fn unmerged_snapshot_rejects_false_agent_merge_report() {
        let result = merge_completion_failed(
            activity_result(),
            ActivityErrorKind::Fatal,
            "Server-side merge completion verification rejected agent output.",
            "agent reported merged=true but GitHub says open",
            Some(snapshot("OPEN", false)),
        );

        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("GitHub says open"));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == MERGE_COMPLETION_VERIFICATION_ARTIFACT
                && artifact.artifact["verified"] == false
                && artifact.artifact["observed_merged"] == false
        }));
    }

    #[test]
    fn mismatched_reported_pr_is_configuration_failure() {
        let job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "codex-default",
            json!({"activity": "merge_pr"}),
        );
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "merging",
            harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1"),
        )
        .with_data(json!({
            "repo": "owner/repo",
            "pr_number": 78,
        }));

        let error = merge_completion_target(&job, Some(&workflow), &activity_result())
            .expect_err("mismatched PR should fail before GitHub verification");

        assert!(error.contains("workflow is bound to PR #78"));
    }
}
