use crate::github_pr_merge::{merge_pull_request, GitHubPrMergeError, GitHubPrMergeOptions};
use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot, value_string, GitHubPrSnapshotArtifacts, GitHubPrSnapshotTarget,
    GITHUB_PR_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ERROR_ARTIFACT,
};
use crate::http::AppState;
use harness_core::config::intake::{
    GitHubAutoMergeConfig, GitHubMergeExecution, GitHubMergeMethod, ResolvedGitHubAutoMergePolicy,
};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, DataProvenance,
    RuntimeJob, WorkflowInstance, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use serde_json::{json, Value};

use super::data_helpers::activity_name;
use super::merge_completion::{
    auto_merge_config, merge_activity_matches, merge_completion_verified, merge_execution_target,
    snapshot_observes_merged,
};

const SERVER_MERGE_EXECUTION_ARTIFACT: &str = "server_merge_execution";
const SERVER_MERGE_EXECUTION_SCHEMA: &str = "harness.github.server_merge_execution.v1";

pub(super) fn server_merge_execution_enabled(
    _state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> bool {
    merge_activity_matches(job, workflow)
}

pub(super) async fn execute_server_merge(
    state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> ActivityResult {
    let activity = activity_name(job);
    let target = match merge_execution_target(job, workflow) {
        Ok(target) => target,
        Err(error) => {
            return server_merge_failed(
                activity,
                None,
                ActivityErrorKind::Configuration,
                "Server-side merge could not resolve the target pull request.",
                error,
                None,
                None,
                "target_invalid",
            );
        }
    };
    let resolved_github_token = crate::github_auth::resolve_github_token(
        state.core.server.config.server.github_token.as_deref(),
    );
    let Some(github_token) = resolved_github_token.as_deref() else {
        let error =
            "server-executed merge requires a GitHub token with pull request merge permission";
        tracing::error!(
            repo = %target.repo_slug,
            pr_number = target.pr_number,
            "server-executed GitHub merge failed due to configuration: {error}"
        );
        return server_merge_failed(
            activity,
            Some(&target),
            ActivityErrorKind::Configuration,
            "Server-side merge configuration is invalid.",
            error,
            None,
            None,
            "missing_github_token",
        );
    };
    let before_snapshot = match fetch_github_pr_snapshot(&target, Some(github_token)).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return server_merge_fetch_failed(
                activity,
                &target,
                "Server-side merge could not read the pull request before merging.",
                "pre_merge_snapshot_failed",
                &error,
            );
        }
    };
    let expected_head_sha = match required_expected_head_sha_for_merge(job, workflow) {
        Ok(expected_head_sha) => expected_head_sha,
        Err(error) => {
            return server_merge_failed(
                activity,
                Some(&target),
                ActivityErrorKind::Configuration,
                "Server-side merge requires a pinned pull request head.",
                error,
                Some(before_snapshot),
                None,
                "head_missing",
            );
        }
    };
    if !snapshot_head_matches_expected(&before_snapshot.normalized_snapshot, &expected_head_sha) {
        return server_merge_head_mismatch(
            activity,
            &target,
            before_snapshot,
            &expected_head_sha,
            None,
        );
    }
    if snapshot_observes_merged(&before_snapshot.normalized_snapshot) {
        return server_merge_succeeded(
            activity,
            &target,
            before_snapshot,
            &expected_head_sha,
            "already_merged_before_merge",
            None,
        );
    }
    let config = auto_merge_config(state);
    let policy = match server_merge_policy(&config, job, workflow) {
        Ok(policy) => policy,
        Err(error) => {
            return server_merge_failed(
                activity,
                Some(&target),
                ActivityErrorKind::Configuration,
                "Server-side merge configuration is invalid.",
                error,
                Some(before_snapshot),
                None,
                "configuration_invalid",
            );
        }
    };
    let expected_base_ref = workflow.and_then(|workflow| {
        crate::http::auto_merge::expected_base_ref_from_workflow_data(&workflow.data)
    });
    if !crate::http::auto_merge::auto_merge_snapshot_satisfies_policy(
        &before_snapshot.normalized_snapshot,
        &policy,
        expected_base_ref.as_deref(),
    ) {
        return server_merge_failed(
            activity,
            Some(&target),
            ActivityErrorKind::Fatal,
            "Server-side merge gate rejected the current pull request state.",
            format!(
                "PR #{} in {} no longer satisfies the auto-merge gate",
                target.pr_number, target.repo_slug
            ),
            Some(before_snapshot),
            None,
            "gate_rejected",
        );
    }
    let merge_call = match merge_pull_request(
        &target,
        Some(github_token),
        &GitHubPrMergeOptions {
            method: policy.method,
            expected_head_sha: Some(expected_head_sha.clone()),
        },
    )
    .await
    {
        Ok(outcome) => json!({
            "status": "ok",
            "merged": outcome.merged,
            "message": outcome.message,
            "sha": outcome.sha,
            "response": outcome.raw,
        }),
        Err(error) => {
            if error.error_kind == ActivityErrorKind::Configuration {
                tracing::error!(
                    repo = %target.repo_slug,
                    pr_number = target.pr_number,
                    "server-executed GitHub merge failed due to configuration: {error}"
                );
            }
            return merge_error_result(
                activity,
                &target,
                Some(github_token),
                &expected_head_sha,
                error,
            )
            .await;
        }
    };
    match fetch_github_pr_snapshot(&target, Some(github_token)).await {
        Ok(snapshot) if snapshot_observes_merged(&snapshot.normalized_snapshot) => {
            server_merge_succeeded(
                activity,
                &target,
                snapshot,
                &expected_head_sha,
                "merged",
                Some(merge_call),
            )
        }
        Ok(snapshot) => server_merge_failed(
            activity,
            Some(&target),
            ActivityErrorKind::ExternalDependency,
            "Server-side merge could not confirm GitHub merged state.",
            format!(
                "GitHub merge API returned success for PR #{} in {}, but the confirmation snapshot was not merged",
                target.pr_number, target.repo_slug
            ),
            Some(snapshot),
            Some(merge_call),
            "confirmation_not_merged",
        ),
        Err(error) => server_merge_fetch_failed(
            activity,
            &target,
            "Server-side merge could not confirm GitHub merged state.",
            "confirmation_snapshot_failed",
            &error,
        ),
    }
}

async fn merge_error_result(
    activity: String,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    expected_head_sha: &str,
    error: GitHubPrMergeError,
) -> ActivityResult {
    match fetch_github_pr_snapshot(target, github_token).await {
        Ok(snapshot) if snapshot_observes_merged(&snapshot.normalized_snapshot) => {
            server_merge_succeeded(
                activity,
                target,
                snapshot,
                expected_head_sha,
                "already_merged_after_merge_error",
                Some(server_merge_error_payload(&error)),
            )
        }
        Ok(snapshot) => server_merge_failed(
            activity,
            Some(target),
            error.error_kind,
            "Server-side merge call failed.",
            error.to_string(),
            Some(snapshot),
            Some(server_merge_error_payload(&error)),
            "merge_call_failed",
        ),
        Err(snapshot_error) => server_merge_failed(
            activity,
            Some(target),
            error.error_kind,
            "Server-side merge call failed and confirmation read also failed.",
            format!("{error}; confirmation read failed: {snapshot_error}"),
            None,
            Some(server_merge_error_payload(&error)),
            "merge_call_and_confirmation_failed",
        ),
    }
}

fn server_merge_head_mismatch(
    activity: String,
    target: &GitHubPrSnapshotTarget,
    snapshot: GitHubPrSnapshotArtifacts,
    expected_head_sha: &str,
    merge_call: Option<Value>,
) -> ActivityResult {
    let observed_head_sha = value_string(snapshot.normalized_snapshot.get("head_oid"))
        .unwrap_or_else(|| "<missing>".to_string());
    ActivityResult::succeeded(
        activity,
        "The pull request head changed after its last scope assessment; a new model assessment is required.",
    )
    .with_signal(ActivitySignal::new(
        "PrHeadChanged",
        json!({
            "repo": target.repo_slug,
            "pr_number": target.pr_number,
            "expected_head_sha": expected_head_sha,
            "observed_head_sha": observed_head_sha,
        }),
    ))
    .with_artifact(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ARTIFACT,
        snapshot.normalized_snapshot,
    ))
    .with_artifact(ActivityArtifact::new(
        GITHUB_PR_SNAPSHOT_ARTIFACT,
        snapshot.raw_pr,
    ))
    .with_artifact(server_merge_execution_artifact(
        Some(target),
        "head_changed",
        Some(format!(
            "PR #{} in {} head {} did not match the scope-assessed head {}",
            target.pr_number, target.repo_slug, observed_head_sha, expected_head_sha
        )),
        merge_call,
    ))
}

fn server_merge_policy(
    config: &GitHubAutoMergeConfig,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<ResolvedGitHubAutoMergePolicy, String> {
    Ok(ResolvedGitHubAutoMergePolicy {
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
    })
}

fn merge_method_for_activity(
    config: &GitHubAutoMergeConfig,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<GitHubMergeMethod, String> {
    let Some(raw) = activity_string(job, workflow, "merge_method") else {
        return Ok(config.method);
    };
    parse_merge_method(&raw).ok_or_else(|| format!("unsupported merge_method `{raw}`"))
}

fn parse_merge_method(raw: &str) -> Option<GitHubMergeMethod> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "squash" => Some(GitHubMergeMethod::Squash),
        "merge" => Some(GitHubMergeMethod::Merge),
        "rebase" => Some(GitHubMergeMethod::Rebase),
        _ => None,
    }
}

fn required_expected_head_sha_for_merge(
    _job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> Result<String, &'static str> {
    workflow
        .filter(|workflow| {
            workflow
                .data_provenance
                .as_ref()
                .and_then(|provenance| provenance.provenance_for("/scope_assessed_head_oid"))
                == Some(DataProvenance::Server)
        })
        .and_then(|workflow| value_string(workflow.data.get("scope_assessed_head_oid")))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or("merge_pr dispatch is missing a server-owned scope_assessed_head_oid")
}

fn activity_string(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    field: &str,
) -> Option<String> {
    value_string(job.input.get(field))
        .or_else(|| workflow.and_then(|workflow| value_string(workflow.data.get(field))))
}

fn activity_bool(
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    field: &str,
) -> Option<bool> {
    job.input
        .get(field)
        .and_then(Value::as_bool)
        .or_else(|| workflow.and_then(|workflow| workflow.data.get(field).and_then(Value::as_bool)))
}

fn snapshot_head_matches_expected(snapshot: &Value, expected_head_sha: &str) -> bool {
    value_string(snapshot.get("head_oid")).is_some_and(|head_oid| head_oid == expected_head_sha)
}

fn server_merge_succeeded(
    activity: String,
    target: &GitHubPrSnapshotTarget,
    snapshot: GitHubPrSnapshotArtifacts,
    expected_head_sha: &str,
    outcome: &str,
    merge_call: Option<Value>,
) -> ActivityResult {
    if !snapshot_head_matches_expected(&snapshot.normalized_snapshot, expected_head_sha) {
        return server_merge_head_mismatch(
            activity,
            target,
            snapshot,
            expected_head_sha,
            merge_call,
        );
    }
    let result = ActivityResult::succeeded(activity, server_merge_success_summary(outcome))
        .with_artifact(server_merge_pull_request_artifact(
            target,
            &snapshot.normalized_snapshot,
        ))
        .with_artifact(server_merge_execution_artifact(
            Some(target),
            outcome,
            None,
            merge_call,
        ));
    merge_completion_verified(result, target, snapshot)
}

fn server_merge_success_summary(outcome: &str) -> &'static str {
    match outcome {
        "already_merged_before_merge" | "already_merged_after_merge_error" => {
            "Pull request was already merged according to GitHub."
        }
        _ => "Server merged the pull request and confirmed GitHub merged state.",
    }
}

fn server_merge_pull_request_artifact(
    target: &GitHubPrSnapshotTarget,
    snapshot: &Value,
) -> ActivityArtifact {
    let pr_url = value_string(snapshot.get("pr_url")).unwrap_or_else(|| {
        format!(
            "https://github.com/{}/pull/{}",
            target.repo_slug, target.pr_number
        )
    });
    ActivityArtifact::new(
        "pull_request",
        json!({
            "pr_number": target.pr_number,
            "pr_url": pr_url,
            "state": snapshot.get("state").cloned().unwrap_or_else(|| json!("MERGED")),
            "merged": true,
            "merge_commit_sha": snapshot.get("merge_commit_sha").cloned().unwrap_or(Value::Null),
            "head_sha": snapshot.get("head_oid").cloned().unwrap_or(Value::Null),
            "server_verified": true,
            "verification_source": "server_github_graphql",
        }),
    )
}

fn server_merge_failed(
    activity: String,
    target: Option<&GitHubPrSnapshotTarget>,
    error_kind: ActivityErrorKind,
    summary: impl Into<String>,
    error: impl Into<String>,
    snapshot: Option<GitHubPrSnapshotArtifacts>,
    merge_call: Option<Value>,
    outcome: &str,
) -> ActivityResult {
    let error = error.into();
    let mut result =
        ActivityResult::failed(activity, summary, error.clone()).with_error_kind(error_kind);
    result.artifacts.push(server_merge_execution_artifact(
        target,
        outcome,
        Some(error),
        merge_call,
    ));
    if let Some(snapshot) = snapshot {
        result.artifacts.push(ActivityArtifact::new(
            SERVER_PR_SNAPSHOT_ARTIFACT,
            snapshot.normalized_snapshot,
        ));
        result.artifacts.push(ActivityArtifact::new(
            GITHUB_PR_SNAPSHOT_ARTIFACT,
            snapshot.raw_pr,
        ));
    }
    result
}

fn server_merge_fetch_failed(
    activity: String,
    target: &GitHubPrSnapshotTarget,
    summary: &str,
    outcome: &str,
    error: &anyhow::Error,
) -> ActivityResult {
    let mut result = server_merge_failed(
        activity,
        Some(target),
        ActivityErrorKind::ExternalDependency,
        summary,
        error.to_string(),
        None,
        None,
        outcome,
    );
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

fn server_merge_execution_artifact(
    target: Option<&GitHubPrSnapshotTarget>,
    outcome: &str,
    reason: Option<String>,
    merge_call: Option<Value>,
) -> ActivityArtifact {
    ActivityArtifact::new(
        SERVER_MERGE_EXECUTION_ARTIFACT,
        json!({
            "schema": SERVER_MERGE_EXECUTION_SCHEMA,
            "executor": "server",
            "repo": target.map(|target| target.repo_slug.as_str()),
            "pr_number": target.map(|target| target.pr_number),
            "outcome": outcome,
            "reason": reason,
            "merge_call": merge_call,
        }),
    )
}

fn server_merge_error_payload(error: &GitHubPrMergeError) -> Value {
    json!({
        "status": "error",
        "error_kind": error.error_kind,
        "message": error.message,
        "status_code": error.status_code,
        "response_body": error.response_body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{ActivityStatus, RuntimeKind, WorkflowSubject};

    fn workflow() -> WorkflowInstance {
        WorkflowInstance::new(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "merging",
            WorkflowSubject::new("issue", "issue:77"),
        )
        .with_server_data(json!({
            "repo": "owner/repo",
            "pr_number": 77,
            "merge_method": "squash",
            "merge_delete_branch": false,
            "merge_require_review_threads_resolved": true,
            "merge_require_clean_merge_state": true,
            "merge_attempted_head_sha": "head-sha",
        }))
    }

    fn job() -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexExec,
            "codex-default",
            json!({
                "activity": "merge_pr",
                "expected_head_sha": "head-sha",
            }),
        )
    }

    fn server_merge_test_snapshot(
        state: &str,
        merged: bool,
        head_sha: &str,
    ) -> GitHubPrSnapshotArtifacts {
        let normalized_snapshot = json!({
            "schema": "harness.github.pr_snapshot.v1",
            "repo": "owner/repo",
            "pr_number": 77,
            "state": state,
            "merged": merged,
            "pr_url": "https://github.com/owner/repo/pull/77",
            "base_ref": "main",
            "head_ref": "feature",
            "head_oid": head_sha,
            "merge_commit_sha": if merged { json!("merge-sha") } else { Value::Null },
            "is_draft": false,
            "merge_state_status": "CLEAN",
            "review_decision": "APPROVED",
            "status_check_rollup_state": "SUCCESS",
            "review_threads_complete": true,
            "active_unresolved_review_threads_count": 0,
        });
        GitHubPrSnapshotArtifacts {
            raw_pr: normalized_snapshot.clone(),
            normalized_snapshot,
        }
    }

    #[test]
    fn server_merge_policy_uses_runtime_command_overrides() {
        let workflow = workflow();
        let policy =
            server_merge_policy(&GitHubAutoMergeConfig::default(), &job(), Some(&workflow))
                .expect("valid policy");

        assert_eq!(policy.method, GitHubMergeMethod::Squash);
        assert!(!policy.delete_branch);
        assert!(policy.require_review_threads_resolved);
        assert!(policy.require_clean_merge_state);
        assert_eq!(policy.merge_execution, GitHubMergeExecution::Server);
    }

    #[test]
    fn server_success_result_contains_verified_pull_request() {
        let target = GitHubPrSnapshotTarget::new("owner/repo", 77).expect("target");
        let result = server_merge_succeeded(
            "merge_pr".to_string(),
            &target,
            server_merge_test_snapshot("MERGED", true, "head-sha"),
            "head-sha",
            "merged",
            Some(json!({ "status": "ok" })),
        );

        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "pull_request"
                && artifact.artifact["merged"] == true
                && artifact.artifact["server_verified"] == true
        }));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == SERVER_MERGE_EXECUTION_ARTIFACT));
    }

    #[test]
    fn stale_head_is_not_mergeable() {
        assert!(snapshot_head_matches_expected(
            &server_merge_test_snapshot("OPEN", false, "fresh-head").normalized_snapshot,
            "fresh-head"
        ));
        assert!(!snapshot_head_matches_expected(
            &server_merge_test_snapshot("OPEN", false, "fresh-head").normalized_snapshot,
            "stale-head"
        ));
        let target = GitHubPrSnapshotTarget::new("owner/repo", 77).expect("target");
        let result = server_merge_succeeded(
            "merge_pr".to_string(),
            &target,
            server_merge_test_snapshot("MERGED", true, "fresh-head"),
            "stale-head",
            "already_merged_before_merge",
            None,
        );
        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(result
            .signals
            .iter()
            .any(|signal| signal.signal_type == "PrHeadChanged"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == SERVER_PR_SNAPSHOT_ARTIFACT));
    }

    #[test]
    fn merge_prefers_scope_assessed_head_over_dispatch_head() {
        let workflow = workflow().with_server_data(json!({
            "scope_assessed_head_oid": "scope-head",
            "pr_head_sha": "dispatch-head",
        }));

        assert_eq!(
            required_expected_head_sha_for_merge(&job(), Some(&workflow)).as_deref(),
            Ok("scope-head")
        );
    }

    #[test]
    fn server_merge_requires_a_server_owned_scope_assessed_head() {
        let missing = RuntimeJob::pending(
            "command-missing",
            RuntimeKind::CodexExec,
            "codex-default",
            json!({"activity": "merge_pr"}),
        );
        let blank = RuntimeJob::pending(
            "command-blank",
            RuntimeKind::CodexExec,
            "codex-default",
            json!({"activity": "merge_pr", "expected_head_sha": "  "}),
        );

        assert!(required_expected_head_sha_for_merge(&missing, None).is_err());
        assert!(required_expected_head_sha_for_merge(&blank, None).is_err());
        assert!(required_expected_head_sha_for_merge(&job(), None).is_err());
    }
}
