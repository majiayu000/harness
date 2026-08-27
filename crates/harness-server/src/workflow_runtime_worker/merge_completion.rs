use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot, value_string, value_u64, GitHubPrSnapshotArtifacts,
    GitHubPrSnapshotTarget, GITHUB_PR_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ERROR_ARTIFACT,
};
use crate::http::AppState;
use harness_core::config::intake::{
    GitHubAutoMergeConfig, GitHubMergeExecution, GitHubMergeMethod, ResolvedGitHubAutoMergePolicy,
};
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
        if !state
            .core
            .server
            .config
            .workflow
            .completion_evidence_enforced
        {
            return result;
        }
        return merge_completion_failed(
            result,
            ActivityErrorKind::Configuration,
            "Server-side merge completion verification is disabled.",
            "Harness refuses to accept an unverified merge_pr completion; enable verify_merge_completion",
            None,
        );
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
        Ok(snapshot) => verify_merge_completion_snapshot(job, workflow, result, &target, snapshot),
        Err(error) => merge_completion_fetch_failed(result, &target, &error),
    }
}

fn verify_merge_completion_snapshot(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    result: ActivityResult,
    target: &GitHubPrSnapshotTarget,
    snapshot: GitHubPrSnapshotArtifacts,
) -> ActivityResult {
    let Some(expected_head_sha) = expected_head_sha_for_merge(job, workflow) else {
        return merge_completion_failed(
            result,
            ActivityErrorKind::Configuration,
            "Server-side merge completion verification requires a pinned pull request head.",
            "merge_pr completion is missing a non-empty expected_head_sha",
            Some(snapshot),
        );
    };
    if !snapshot_observes_merged(&snapshot.normalized_snapshot) {
        return merge_completion_failed(
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
        );
    }
    if !snapshot_head_matches_expected(&snapshot.normalized_snapshot, &expected_head_sha) {
        let observed_head_sha = value_string(snapshot.normalized_snapshot.get("head_oid"))
            .unwrap_or_else(|| "<missing>".to_string());
        return merge_completion_failed(
            result,
            ActivityErrorKind::Fatal,
            "Server-side merge completion verification rejected a stale pull request head.",
            &format!(
                "agent reported merged=true for PR #{} in {}, but GitHub head {} did not match expected_head_sha {}",
                target.pr_number, target.repo_slug, observed_head_sha, expected_head_sha
            ),
            Some(snapshot),
        );
    }
    if !crate::http::auto_merge::snapshot_base_ref_matches_expected(
        &snapshot.normalized_snapshot,
        target.expected_base_ref.as_deref(),
    ) {
        let observed_base_ref = value_string(snapshot.normalized_snapshot.get("base_ref"))
            .unwrap_or_else(|| "<missing>".to_string());
        return merge_completion_failed(
            result,
            ActivityErrorKind::Fatal,
            "Server-side merge completion verification rejected an unauthorized base branch.",
            &format!(
                "agent reported merged=true for PR #{} in {}, but GitHub base {} did not match expected_base_ref {}",
                target.pr_number,
                target.repo_slug,
                observed_base_ref,
                target.expected_base_ref.as_deref().unwrap_or("<missing>")
            ),
            Some(snapshot),
        );
    }
    merge_completion_verified(result, target, snapshot)
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

pub(super) fn server_merge_policy(
    config: &GitHubAutoMergeConfig,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<ResolvedGitHubAutoMergePolicy, String> {
    let policy = ResolvedGitHubAutoMergePolicy {
        enabled: true,
        method: merge_method_for_activity(config, job, workflow)?,
        delete_branch: activity_bool(job, workflow, "delete_branch")
            .or_else(|| activity_bool(job, workflow, "merge_delete_branch"))
            .unwrap_or(config.delete_branch),
        require_review_threads_resolved: activity_bool(
            job,
            workflow,
            "require_review_threads_resolved",
        )
        .or_else(|| activity_bool(job, workflow, "merge_require_review_threads_resolved"))
        .unwrap_or(config.require_review_threads_resolved),
        require_clean_merge_state: activity_bool(job, workflow, "require_clean_merge_state")
            .or_else(|| activity_bool(job, workflow, "merge_require_clean_merge_state"))
            .unwrap_or(config.require_clean_merge_state),
        merge_execution: GitHubMergeExecution::Server,
        verify_merge_completion: config.verify_merge_completion,
    };
    if policy.delete_branch {
        return Err(
            "server-owned merge cannot safely delete a branch because GitHub does not provide an expected-SHA compare-and-delete operation for refs"
                .to_string(),
        );
    }
    Ok(policy)
}

pub(super) fn required_expected_head_sha_for_merge(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<String, String> {
    let command_head = job
        .input
        .pointer("/command/expected_head_sha")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "merge_pr command is missing expected_head_sha".to_string())?;
    let trusted_head = workflow
        .and_then(crate::workflow_runtime_pr_feedback::trusted_merge_head_sha)
        .ok_or_else(|| "merge_pr dispatch is missing a trusted workflow head".to_string())?;
    if command_head != trusted_head {
        return Err(format!(
            "merge_pr command head `{command_head}` does not match trusted workflow head `{trusted_head}`"
        ));
    }
    Ok(trusted_head)
}

pub(super) fn snapshot_head_matches_expected(snapshot: &Value, expected_head_sha: &str) -> bool {
    value_string(snapshot.get("head_oid")).is_some_and(|head_oid| head_oid == expected_head_sha)
}

fn merge_method_for_activity(
    config: &GitHubAutoMergeConfig,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<GitHubMergeMethod, String> {
    let Some(raw) = activity_string(job, workflow, "merge_method") else {
        return Ok(config.method);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "squash" => Ok(GitHubMergeMethod::Squash),
        "merge" => Ok(GitHubMergeMethod::Merge),
        "rebase" => Ok(GitHubMergeMethod::Rebase),
        _ => Err(format!("unsupported merge_method `{raw}`")),
    }
}

fn activity_string(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    field: &str,
) -> Option<String> {
    value_string(job.input.pointer(&format!("/command/{field}")))
        .or_else(|| value_string(job.input.get(field)))
        .or_else(|| workflow.and_then(|workflow| value_string(workflow.data.get(field))))
}

fn activity_bool(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    field: &str,
) -> Option<bool> {
    job.input
        .pointer(&format!("/command/{field}"))
        .and_then(Value::as_bool)
        .or_else(|| job.input.get(field).and_then(Value::as_bool))
        .or_else(|| workflow.and_then(|workflow| workflow.data.get(field).and_then(Value::as_bool)))
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
            .map(|workflow| {
                workflow.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
                    && workflow.state == "merging"
            })
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

fn expected_head_sha_for_merge(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Option<String> {
    activity_string(job, workflow, "expected_head_sha")
        .or_else(|| activity_string(job, workflow, "merge_attempted_head_sha"))
        .or_else(|| activity_string(job, workflow, "pr_head_sha"))
        .or_else(|| activity_string(job, workflow, "head_sha"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
        copy_snapshot_field(object, snapshot, "pr_url");
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
            "base_ref": "main",
            "merge_commit_sha": if merged { json!("merge-sha") } else { Value::Null },
        });
        GitHubPrSnapshotArtifacts {
            raw_pr: normalized_snapshot.clone(),
            normalized_snapshot,
        }
    }

    #[test]
    fn verified_merge_enriches_pull_request_with_server_evidence() {
        let mut result = activity_result();
        result.artifacts[0].artifact["pr_url"] = json!("https://github.com/attacker/repo/pull/77");
        let result = merge_completion_verified(result, &target(), snapshot("MERGED", true));

        assert_eq!(result.status, ActivityStatus::Succeeded);
        let pull_request = result
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_type == "pull_request")
            .expect("pull request artifact");
        assert_eq!(pull_request.artifact["server_verified"], true);
        assert_eq!(
            pull_request.artifact["pr_url"],
            "https://github.com/owner/repo/pull/77"
        );
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

    fn job_with_expected_head(expected_head_sha: Option<&str>) -> RuntimeJob {
        let mut input = json!({"activity": "merge_pr"});
        if let Some(expected_head_sha) = expected_head_sha {
            input["expected_head_sha"] = json!(expected_head_sha);
        }
        RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::CodexExec,
            "codex-default",
            input,
        )
    }

    #[test]
    fn merged_snapshot_requires_a_pinned_dispatched_head() {
        let result = verify_merge_completion_snapshot(
            &job_with_expected_head(None),
            None,
            activity_result(),
            &target(),
            snapshot("MERGED", true),
        );

        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("missing a non-empty expected_head_sha"));
    }

    #[test]
    fn merged_snapshot_rejects_a_head_other_than_the_dispatched_head() {
        let result = verify_merge_completion_snapshot(
            &job_with_expected_head(Some("reviewed-head")),
            None,
            activity_result(),
            &target(),
            snapshot("MERGED", true),
        );

        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("did not match expected_head_sha reviewed-head"));
    }

    #[test]
    fn merged_snapshot_accepts_the_pinned_dispatched_head() {
        let result = verify_merge_completion_snapshot(
            &job_with_expected_head(Some("server-head")),
            None,
            activity_result(),
            &target(),
            snapshot("MERGED", true),
        );

        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == MERGE_COMPLETION_VERIFICATION_ARTIFACT
                && artifact.artifact["verified"] == true
        }));
    }

    #[test]
    fn merged_snapshot_rejects_an_unauthorized_base_branch() {
        let target = target().with_expected_base_ref("release");
        let result = verify_merge_completion_snapshot(
            &job_with_expected_head(Some("server-head")),
            None,
            activity_result(),
            &target,
            snapshot("MERGED", true),
        );

        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("did not match expected_base_ref release"));
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
        .with_server_data(json!({
            "repo": "owner/repo",
            "pr_number": 78,
        }));

        let error = merge_completion_target(&job, Some(&workflow), &activity_result())
            .expect_err("mismatched PR should fail before GitHub verification");

        assert!(error.contains("workflow is bound to PR #78"));
    }
}
