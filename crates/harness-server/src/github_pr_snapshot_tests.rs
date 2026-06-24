use super::*;

fn ready_pr() -> Value {
    json!({
        "number": 77,
        "state": "OPEN",
        "url": "https://github.com/owner/repo/pull/77",
        "title": "Ready PR",
        "baseRefName": "main",
        "headRefName": "feature",
        "headRefOid": "abc123",
        "isDraft": false,
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "APPROVED",
        "statusCheckRollup": {"state": "SUCCESS"},
        "reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {"id": "resolved", "path": "src/lib.rs", "line": 1, "isResolved": true, "isOutdated": false}
            ]
        },
        "files": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {"path": "src/lib.rs", "additions": 3, "deletions": 1, "changeType": "MODIFIED"}
            ]
        },
        "closingIssuesReferences": {
            "nodes": [
                {"number": 12, "url": "https://github.com/owner/repo/issues/12"}
            ]
        }
    })
}

#[test]
fn maps_graphql_pr_to_runtime_snapshot_artifact() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let snapshot = normalize_github_pr_snapshot(&target, &ready_pr()).unwrap();

    assert_eq!(snapshot["schema"], SERVER_PR_SNAPSHOT_SCHEMA);
    assert_eq!(snapshot["snapshot_source"], "server_github_graphql");
    assert_eq!(snapshot["repo"], "owner/repo");
    assert_eq!(snapshot["pr_number"], 77);
    assert_eq!(snapshot["state"], "OPEN");
    assert_eq!(snapshot["pr_url"], "https://github.com/owner/repo/pull/77");
    assert_eq!(snapshot["head_oid"], "abc123");
    assert_eq!(snapshot["status_check_rollup_state"], "SUCCESS");
    assert_eq!(snapshot["merge_state_status"], "CLEAN");
    assert_eq!(snapshot["review_decision"], "APPROVED");
    assert_eq!(snapshot["is_draft"], false);
    assert_eq!(snapshot["active_unresolved_review_threads_count"], 0);
    assert_eq!(snapshot["changed_files"][0]["path"], "src/lib.rs");
    assert_eq!(snapshot["closing_issues"][0]["number"], 12);
    assert_eq!(snapshot["review_threads_complete"], true);
}

#[test]
fn classifies_ready_snapshot_without_agent_judgment() -> anyhow::Result<()> {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;
    let snapshot = normalize_github_pr_snapshot(&target, &ready_pr())?;

    assert_eq!(
        pr_readiness_for_snapshot(&snapshot),
        PrReadiness::ReadyToMerge
    );
    Ok(())
}

#[test]
fn wrong_base_ref_blocks_ready_snapshot() -> anyhow::Result<()> {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?.with_expected_base_ref("main");
    let mut pr = ready_pr();
    pr["baseRefName"] = json!("release");
    let snapshot = normalize_github_pr_snapshot(&target, &pr)?;

    assert_eq!(snapshot["expected_base_ref"], "main");
    assert_eq!(snapshot["base_ref"], "release");
    assert_eq!(
        pr_readiness_for_snapshot(&snapshot),
        PrReadiness::NeedsFeedbackRepair
    );
    assert_eq!(
        pr_feedback_signal_for_snapshot(&snapshot).signal_type,
        "FeedbackFound"
    );
    Ok(())
}

#[test]
fn unknown_expected_base_does_not_block_ready_snapshot() -> anyhow::Result<()> {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;
    let mut pr = ready_pr();
    pr["baseRefName"] = json!("release");
    let snapshot = normalize_github_pr_snapshot(&target, &pr)?;

    assert_eq!(snapshot["base_ref"], "release");
    assert!(snapshot["expected_base_ref"].is_null());
    assert_eq!(
        pr_readiness_for_snapshot(&snapshot),
        PrReadiness::ReadyToMerge
    );
    Ok(())
}

#[test]
fn classifies_merged_and_closed_snapshots() -> anyhow::Result<()> {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;
    let mut merged = ready_pr();
    merged["state"] = json!("MERGED");
    let merged = normalize_github_pr_snapshot(&target, &merged)?;
    assert_eq!(pr_readiness_for_snapshot(&merged), PrReadiness::Merged);

    let mut closed = ready_pr();
    closed["state"] = json!("CLOSED");
    let closed = normalize_github_pr_snapshot(&target, &closed)?;
    assert_eq!(
        pr_readiness_for_snapshot(&closed),
        PrReadiness::ClosedUnmerged
    );
    Ok(())
}

#[test]
fn pr_remote_fact_hash_ignores_observed_at() -> anyhow::Result<()> {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77)?;
    let mut left = normalize_github_pr_snapshot(&target, &ready_pr())?;
    let mut right = left.clone();
    left["observed_at"] = json!("2026-06-24T00:00:00Z");
    right["observed_at"] = json!("2026-06-24T00:01:00Z");

    let left = GitHubPrSnapshotArtifacts {
        raw_pr: ready_pr(),
        normalized_snapshot: left,
    }
    .remote_fact_snapshot()?;
    let right = GitHubPrSnapshotArtifacts {
        raw_pr: ready_pr(),
        normalized_snapshot: right,
    }
    .remote_fact_snapshot()?;

    assert_eq!(left.fact_hash, right.fact_hash);
    assert_ne!(left.facts["observed_at"], right.facts["observed_at"]);
    Ok(())
}

#[test]
fn ready_pr_emits_pr_ready_to_merge_signal() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let snapshot = normalize_github_pr_snapshot(&target, &ready_pr()).unwrap();
    let artifacts = GitHubPrSnapshotArtifacts {
        raw_pr: ready_pr(),
        normalized_snapshot: snapshot,
    };

    let result = artifacts.activity_result("inspect_pr_feedback");

    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Succeeded
    );
    assert_eq!(result.signals[0].signal_type, "PrReadyToMerge");
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == SERVER_PR_SNAPSHOT_ARTIFACT));
    assert!(result
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_type == PR_FEEDBACK_SNAPSHOT_ARTIFACT));
}

#[test]
fn unresolved_review_threads_emit_blocking_feedback() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let mut pr = ready_pr();
    pr["reviewThreads"]["nodes"] = json!([
        {"id": "thread-1", "path": "src/lib.rs", "line": 10, "isResolved": false, "isOutdated": false}
    ]);
    let snapshot = normalize_github_pr_snapshot(&target, &pr).unwrap();

    let signal = pr_feedback_signal_for_snapshot(&snapshot);

    assert_eq!(snapshot["active_unresolved_review_threads_count"], 1);
    assert_eq!(signal.signal_type, "FeedbackFound");
}

#[test]
fn incomplete_review_thread_page_emits_blocking_feedback() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let mut pr = ready_pr();
    pr["reviewThreads"]["pageInfo"]["hasNextPage"] = json!(true);
    let snapshot = normalize_github_pr_snapshot(&target, &pr).unwrap();

    let signal = pr_feedback_signal_for_snapshot(&snapshot);

    assert_eq!(snapshot["review_threads_complete"], false);
    assert_eq!(signal.signal_type, "FeedbackFound");
}

#[test]
fn missing_review_thread_completeness_emits_blocking_feedback() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let mut snapshot = normalize_github_pr_snapshot(&target, &ready_pr()).unwrap();
    let Some(snapshot_object) = snapshot.as_object_mut() else {
        panic!("snapshot should be an object");
    };
    snapshot_object.remove("review_threads_complete");

    let signal = pr_feedback_signal_for_snapshot(&snapshot);

    assert_eq!(signal.signal_type, "FeedbackFound");
}

#[test]
fn dirty_merge_state_emits_blocking_feedback() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let mut pr = ready_pr();
    pr["mergeStateStatus"] = json!("DIRTY");
    let snapshot = normalize_github_pr_snapshot(&target, &pr).unwrap();

    let signal = pr_feedback_signal_for_snapshot(&snapshot);

    assert_eq!(snapshot["merge_state_status"], "DIRTY");
    assert_eq!(signal.signal_type, "FeedbackFound");
}

#[test]
fn github_graphql_error_is_failed_external_dependency() {
    let target = GitHubPrSnapshotTarget::new("owner/repo", 77).unwrap();
    let error = anyhow::anyhow!("GitHub PR snapshot query returned errors");

    let result = github_pr_snapshot_failure_result("inspect_pr_feedback", Some(&target), &error);

    assert_eq!(
        result.status,
        harness_workflow::runtime::ActivityStatus::Failed
    );
    assert_eq!(
        result.error_kind,
        Some(ActivityErrorKind::ExternalDependency)
    );
    assert_eq!(
        result.artifacts[0].artifact_type,
        SERVER_PR_SNAPSHOT_ERROR_ARTIFACT
    );
}
