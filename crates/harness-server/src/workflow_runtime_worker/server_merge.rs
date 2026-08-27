use crate::github_pr_merge::{merge_pull_request, GitHubPrMergeError, GitHubPrMergeOptions};
use crate::github_pr_snapshot::{
    fetch_github_pr_snapshot, value_string, GitHubPrSnapshotArtifacts, GitHubPrSnapshotTarget,
    GITHUB_PR_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ERROR_ARTIFACT,
};
use crate::http::AppState;
#[cfg(test)]
use harness_core::config::intake::{
    GitHubAutoMergeConfig, GitHubMergeExecution, GitHubMergeMethod,
};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, RuntimeJob,
    WorkflowInstance, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use serde_json::{json, Value};

use super::data_helpers::activity_name;
use super::merge_completion::{
    auto_merge_config, merge_activity_matches, merge_completion_verified, merge_execution_target,
    required_expected_head_sha_for_merge, server_merge_policy, snapshot_head_matches_expected,
    snapshot_observes_merged,
};

const SERVER_MERGE_EXECUTION_ARTIFACT: &str = "server_merge_execution";
const SERVER_MERGE_EXECUTION_SCHEMA: &str = "harness.github.server_merge_execution.v1";

pub(super) fn server_merge_execution_enabled(
    _state: &AppState,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
) -> bool {
    merge_activity_matches(job, workflow) && workflow_uses_server_merge(workflow)
}

fn workflow_uses_server_merge(workflow: Option<&WorkflowInstance>) -> bool {
    workflow.is_some_and(harness_workflow::runtime::workflow_uses_server_merge)
}

pub(super) async fn execute_server_merge(
    state: &AppState,
    job: &RuntimeJob,
    _workflow: Option<&WorkflowInstance>,
) -> ActivityResult {
    let activity = activity_name(job);
    let authorized_workflow =
        match super::executor::server_owned::current_merge_authorization(state, job).await {
            Ok(workflow) => workflow,
            Err(error) => {
                return server_merge_failed(
                    activity,
                    None,
                    ActivityErrorKind::Fatal,
                    "Server-side merge authorization is no longer current.",
                    error,
                    None,
                    None,
                    "authorization_stale",
                );
            }
        };
    let workflow = Some(&authorized_workflow);
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
        Err(_) => {
            return server_merge_head_mismatch(
                activity,
                &target,
                before_snapshot,
                "<missing-scope-assessment>",
                None,
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
    if snapshot_observes_merged(&before_snapshot.normalized_snapshot) {
        if !crate::http::auto_merge::snapshot_base_ref_matches_expected(
            &before_snapshot.normalized_snapshot,
            expected_base_ref.as_deref(),
        ) {
            return server_merge_failed(
                activity,
                Some(&target),
                ActivityErrorKind::Fatal,
                "Server-side merge observed the pull request on an unauthorized base branch.",
                "the merged pull request base does not match expected_base_ref",
                Some(before_snapshot),
                None,
                "merged_base_mismatch",
            );
        }
        return super::executor::server_owned::finish_server_merge(
            state,
            job,
            activity,
            &target,
            before_snapshot,
            &expected_head_sha,
            "already_merged_before_merge",
            None,
            policy.delete_branch,
            Some(github_token),
        )
        .await;
    }
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
    if expected_base_ref.is_some() {
        return server_merge_failed(
            activity,
            Some(&target),
            ActivityErrorKind::Configuration,
            "Server-side merge cannot atomically bind the authorized base branch.",
            "GitHub's pull request merge API accepts an expected head SHA but no expected base ref; automated merge is disabled while this authorization is pinned",
            Some(before_snapshot),
            None,
            "atomic_base_precondition_unavailable",
        );
    }
    let mutation_fence =
        match super::executor::server_owned::fenced_merge_authorization(state, job).await {
            Ok(fence) => fence,
            Err(error) => {
                return server_merge_failed(
                    activity,
                    Some(&target),
                    ActivityErrorKind::Fatal,
                    "Server-side merge authorization changed before mutation.",
                    error,
                    Some(before_snapshot),
                    None,
                    "authorization_changed",
                );
            }
        };
    if required_expected_head_sha_for_merge(job, Some(&mutation_fence.workflow)).as_deref()
        != Ok(expected_head_sha.as_str())
    {
        if let Err(error) = mutation_fence.release().await {
            return mutation_fence_release_failed(activity, &target, error);
        }
        return server_merge_head_mismatch(
            activity,
            &target,
            before_snapshot,
            &expected_head_sha,
            None,
        );
    }
    let mutation_snapshot = match fetch_github_pr_snapshot(&target, Some(github_token)).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if let Err(release_error) = mutation_fence.release().await {
                return mutation_fence_release_failed(activity, &target, release_error);
            }
            return server_merge_fetch_failed(
                activity,
                &target,
                "Server-side merge could not refresh the pull request inside its authorization fence.",
                "mutation_snapshot_failed",
                &error,
            );
        }
    };
    if !snapshot_head_matches_expected(&mutation_snapshot.normalized_snapshot, &expected_head_sha) {
        if let Err(error) = mutation_fence.release().await {
            return mutation_fence_release_failed(activity, &target, error);
        }
        return server_merge_head_mismatch(
            activity,
            &target,
            mutation_snapshot,
            &expected_head_sha,
            None,
        );
    }
    let mutation_expected_base_ref = crate::http::auto_merge::expected_base_ref_from_workflow_data(
        &mutation_fence.workflow.data,
    );
    if !crate::http::auto_merge::auto_merge_snapshot_satisfies_policy(
        &mutation_snapshot.normalized_snapshot,
        &policy,
        mutation_expected_base_ref.as_deref(),
    ) {
        if let Err(error) = mutation_fence.release().await {
            return mutation_fence_release_failed(activity, &target, error);
        }
        return server_merge_failed(
            activity,
            Some(&target),
            ActivityErrorKind::Fatal,
            "Server-side merge gate changed inside the mutation authorization fence.",
            format!(
                "PR #{} in {} changed after the initial merge gate snapshot",
                target.pr_number, target.repo_slug
            ),
            Some(mutation_snapshot),
            None,
            "mutation_gate_changed",
        );
    }
    if let Err(error) =
        super::executor::server_owned::validate_fenced_merge_authorization(job, &mutation_fence)
    {
        if let Err(release_error) = mutation_fence.release().await {
            return mutation_fence_release_failed(activity, &target, release_error);
        }
        return server_merge_failed(
            activity,
            Some(&target),
            ActivityErrorKind::Fatal,
            "Server-side merge authorization expired before mutation.",
            error,
            Some(mutation_snapshot),
            None,
            "authorization_expired",
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
        Ok(outcome) => {
            if let Err(error) = mutation_fence.release().await {
                return mutation_fence_release_failed(activity, &target, error);
            }
            json!({
                "status": "ok",
                "merged": outcome.merged,
                "message": outcome.message,
                "sha": outcome.sha,
                "response": outcome.raw,
            })
        }
        Err(error) => {
            if let Err(release_error) = mutation_fence.release().await {
                return mutation_fence_release_failed(activity, &target, release_error);
            }
            if error.error_kind == ActivityErrorKind::Configuration {
                tracing::error!(
                    repo = %target.repo_slug,
                    pr_number = target.pr_number,
                    "server-executed GitHub merge failed due to configuration: {error}"
                );
            }
            return merge_error_result(
                state,
                job,
                activity,
                &target,
                Some(github_token),
                &expected_head_sha,
                policy.delete_branch,
                error,
            )
            .await;
        }
    };
    match fetch_github_pr_snapshot(&target, Some(github_token)).await {
        Ok(snapshot) if snapshot_observes_merged(&snapshot.normalized_snapshot) => {
            super::executor::server_owned::finish_server_merge(
                state,
                job,
                activity,
                &target,
                snapshot,
                &expected_head_sha,
                "merged",
                Some(merge_call),
                policy.delete_branch,
                Some(github_token),
            )
            .await
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

fn mutation_fence_release_failed(
    activity: String,
    target: &GitHubPrSnapshotTarget,
    error: anyhow::Error,
) -> ActivityResult {
    server_merge_failed(
        activity,
        Some(target),
        ActivityErrorKind::ExternalDependency,
        "Server-side merge could not release its mutation authorization fence.",
        error.to_string(),
        None,
        None,
        "mutation_fence_release_failed",
    )
}

async fn merge_error_result(
    state: &AppState,
    job: &RuntimeJob,
    activity: String,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    expected_head_sha: &str,
    delete_branch: bool,
    error: GitHubPrMergeError,
) -> ActivityResult {
    match fetch_github_pr_snapshot(target, github_token).await {
        Ok(snapshot) if snapshot_observes_merged(&snapshot.normalized_snapshot) => {
            super::executor::server_owned::finish_server_merge(
                state,
                job,
                activity,
                target,
                snapshot,
                expected_head_sha,
                "already_merged_after_merge_error",
                Some(server_merge_error_payload(&error)),
                delete_branch,
                github_token,
            )
            .await
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

pub(in crate::workflow_runtime_worker) fn server_merge_head_mismatch(
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

pub(in crate::workflow_runtime_worker) fn server_merge_succeeded(
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

pub(in crate::workflow_runtime_worker) fn server_merge_failed(
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
#[path = "server_merge_tests.rs"]
mod tests;
