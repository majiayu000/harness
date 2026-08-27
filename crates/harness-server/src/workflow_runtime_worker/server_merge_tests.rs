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
        "pr_head_sha": "head-sha",
    }))
}

fn current_workflow(scope_head: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "merging",
        WorkflowSubject::new("issue", "issue:current-77"),
    )
    .with_server_data(json!({
        "definition_hash": harness_workflow::runtime::github_issue_pr_definition_hash(),
        "repo": "owner/repo",
        "pr_number": 77,
        "scope_assessed_head_oid": scope_head,
    }))
}

fn job() -> RuntimeJob {
    RuntimeJob::pending(
        "command-1",
        RuntimeKind::CodexExec,
        "codex-default",
        json!({
            "activity": "merge_pr",
            "command": {
                "activity": "merge_pr",
                "expected_head_sha": "head-sha",
                "merge_method": "squash",
                "delete_branch": false,
            },
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
    let policy = server_merge_policy(&GitHubAutoMergeConfig::default(), &job(), Some(&workflow))
        .expect("valid policy");

    assert_eq!(policy.method, GitHubMergeMethod::Squash);
    assert!(!policy.delete_branch);
    assert!(policy.require_review_threads_resolved);
    assert!(policy.require_clean_merge_state);
    assert_eq!(policy.merge_execution, GitHubMergeExecution::Server);
}

#[test]
fn server_merge_interception_requires_explicit_server_execution() {
    let mut server = current_workflow("head-sha");
    server.data["merge_execution"] = json!("server");
    let mut agent = server.clone();
    agent.data["merge_execution"] = json!("agent");

    assert!(workflow_uses_server_merge(Some(&server)));
    assert!(!workflow_uses_server_merge(Some(&agent)));
}

#[test]
fn server_merge_policy_rejects_non_atomic_branch_deletion() {
    let workflow = workflow();
    let mut job = job();
    job.input["command"]["delete_branch"] = json!(true);

    let error = server_merge_policy(&GitHubAutoMergeConfig::default(), &job, Some(&workflow))
        .expect_err("non-atomic branch deletion must fail before merge");

    assert!(error.contains("expected-SHA compare-and-delete"));
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
fn merge_requires_command_head_to_match_the_versioned_trusted_head() {
    let legacy = workflow();
    assert_eq!(
        required_expected_head_sha_for_merge(&job(), Some(&legacy)).as_deref(),
        Ok("head-sha")
    );

    let matching = current_workflow("head-sha");

    assert_eq!(
        required_expected_head_sha_for_merge(&job(), Some(&matching)).as_deref(),
        Ok("head-sha")
    );
    let mismatch = current_workflow("different-head");
    assert!(required_expected_head_sha_for_merge(&job(), Some(&mismatch)).is_err());
}

#[test]
fn server_merge_requires_a_trusted_workflow_head() {
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
        json!({"activity": "merge_pr", "command": {"expected_head_sha": "  "}}),
    );

    assert!(required_expected_head_sha_for_merge(&missing, None).is_err());
    assert!(required_expected_head_sha_for_merge(&blank, None).is_err());
    assert!(required_expected_head_sha_for_merge(&job(), None).is_err());
}
