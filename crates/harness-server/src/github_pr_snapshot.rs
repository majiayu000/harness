use anyhow::Context;
use chrono::{SecondsFormat, Utc};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal,
    PR_FEEDBACK_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const SERVER_PR_SNAPSHOT_SCHEMA: &str = "harness.github.pr_snapshot.v1";
pub(crate) const GITHUB_PR_SNAPSHOT_ARTIFACT: &str = "github_pr_snapshot";
pub(crate) const SERVER_PR_SNAPSHOT_ERROR_ARTIFACT: &str = "server_pr_snapshot_error";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubPrSnapshotTarget {
    pub repo_slug: String,
    pub pr_number: u64,
}

impl GitHubPrSnapshotTarget {
    pub(crate) fn new(repo_slug: impl Into<String>, pr_number: u64) -> anyhow::Result<Self> {
        let repo_slug = repo_slug.into();
        let Some((owner, repo)) = repo_slug.split_once('/') else {
            anyhow::bail!("invalid GitHub repo slug `{repo_slug}`");
        };
        if owner.trim().is_empty() || repo.trim().is_empty() {
            anyhow::bail!("invalid GitHub repo slug `{repo_slug}`");
        }
        if pr_number == 0 {
            anyhow::bail!("PR number must be non-zero");
        }
        Ok(Self {
            repo_slug,
            pr_number,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GitHubPrSnapshotArtifacts {
    pub raw_pr: Value,
    pub normalized_snapshot: Value,
}

impl GitHubPrSnapshotArtifacts {
    pub(crate) fn activity_result(&self, activity: &str) -> ActivityResult {
        let signal = pr_feedback_signal_for_snapshot(&self.normalized_snapshot);
        let summary = pr_feedback_summary(signal.signal_type.as_str(), &self.normalized_snapshot);
        ActivityResult::succeeded(activity, summary)
            .with_artifact(ActivityArtifact::new(
                SERVER_PR_SNAPSHOT_ARTIFACT,
                self.normalized_snapshot.clone(),
            ))
            .with_artifact(ActivityArtifact::new(
                PR_FEEDBACK_SNAPSHOT_ARTIFACT,
                self.normalized_snapshot.clone(),
            ))
            .with_artifact(ActivityArtifact::new(
                GITHUB_PR_SNAPSHOT_ARTIFACT,
                self.raw_pr.clone(),
            ))
            .with_signal(signal)
    }
}

pub(crate) async fn fetch_github_pr_snapshot(
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubPrSnapshotArtifacts> {
    let client = reqwest::Client::new();
    fetch_github_pr_snapshot_with_client(&client, target, github_token).await
}

async fn fetch_github_pr_snapshot_with_client(
    client: &reqwest::Client,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubPrSnapshotArtifacts> {
    let raw_pr = fetch_github_pr_snapshot_value(client, target, github_token).await?;
    let normalized_snapshot = normalize_github_pr_snapshot(target, &raw_pr)?;
    Ok(GitHubPrSnapshotArtifacts {
        raw_pr,
        normalized_snapshot,
    })
}

async fn fetch_github_pr_snapshot_value(
    client: &reqwest::Client,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
) -> anyhow::Result<Value> {
    let (owner, repo) = target
        .repo_slug
        .split_once('/')
        .context("validated repo slug should contain owner and repo")?;
    let query = r#"
        query HarnessPrSnapshot($owner: String!, $repo: String!, $pr: Int!) {
          repository(owner: $owner, name: $repo) {
            pullRequest(number: $pr) {
              number
              url
              title
              baseRefName
              headRefName
              headRefOid
              isDraft
              mergeStateStatus
              reviewDecision
              statusCheckRollup {
                state
              }
              reviewThreads(first: 100) {
                pageInfo {
                  hasNextPage
                  endCursor
                }
                nodes {
                  id
                  path
                  line
                  isResolved
                  isOutdated
                  comments(first: 5) {
                    nodes {
                      author { login }
                      body
                      publishedAt
                    }
                  }
                }
              }
              files(first: 100) {
                pageInfo {
                  hasNextPage
                  endCursor
                }
                nodes {
                  path
                  additions
                  deletions
                  changeType
                }
              }
            }
          }
        }
    "#;

    let mut request = client
        .post(GITHUB_GRAPHQL_URL)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "harness-server")
        .json(&json!({
            "query": query,
            "variables": {
                "owner": owner,
                "repo": repo,
                "pr": target.pr_number as i64,
            }
        }));
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }

    let response = tokio::time::timeout(Duration::from_secs(15), request.send()).await??;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("GitHub PR snapshot query failed with status {status}: {body}");
    }
    let parsed: GitHubPrSnapshotGraphQlResponse =
        serde_json::from_str(&body).context("GitHub PR snapshot response was invalid JSON")?;
    if let Some(errors) = parsed.errors.filter(|errors| !errors_is_empty(errors)) {
        anyhow::bail!("GitHub PR snapshot query returned errors: {errors}");
    }
    parsed
        .data
        .and_then(|data| data.get("repository").cloned())
        .and_then(|repository| repository.get("pullRequest").cloned())
        .filter(|pr| !pr.is_null())
        .ok_or_else(|| anyhow::anyhow!("GitHub PR snapshot query returned no PR data"))
}

#[derive(Debug, Deserialize)]
struct GitHubPrSnapshotGraphQlResponse {
    data: Option<Value>,
    errors: Option<Value>,
}

pub(crate) fn errors_is_empty(errors: &Value) -> bool {
    errors.as_array().is_some_and(Vec::is_empty)
}

fn normalize_github_pr_snapshot(
    target: &GitHubPrSnapshotTarget,
    pr: &Value,
) -> anyhow::Result<Value> {
    let pr_number = value_u64(pr.get("number")).context("GitHub PR snapshot missing number")?;
    let pr_url = value_string(pr.get("url")).context("GitHub PR snapshot missing url")?;
    let title = value_string(pr.get("title"));
    let base_ref = value_string(pr.get("baseRefName"));
    let head_ref = value_string(pr.get("headRefName"));
    let head_oid = value_string(pr.get("headRefOid"));
    let is_draft = pr.get("isDraft").and_then(Value::as_bool);
    let merge_state_status = value_string(pr.get("mergeStateStatus"));
    let review_decision = value_string(pr.get("reviewDecision"));
    let status_check_rollup_state = pr
        .get("statusCheckRollup")
        .and_then(|rollup| rollup.get("state"))
        .and_then(|value| value_string(Some(value)));
    let active_threads = active_unresolved_review_threads(pr);
    let changed_files = changed_files(pr);
    let observed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let review_threads_complete = !connection_has_next_page(pr, "reviewThreads");
    let changed_files_complete = !connection_has_next_page(pr, "files");

    Ok(json!({
        "schema": SERVER_PR_SNAPSHOT_SCHEMA,
        "snapshot_source": "server_github_graphql",
        "observed_at": observed_at,
        "repo": target.repo_slug,
        "pr_number": pr_number,
        "pr_url": pr_url,
        "url": pr_url,
        "title": title,
        "base_ref": base_ref,
        "baseRefName": base_ref,
        "head_ref": head_ref,
        "headRefName": head_ref,
        "head_oid": head_oid,
        "headRefOid": head_oid,
        "is_draft": is_draft,
        "isDraft": is_draft,
        "merge_state_status": merge_state_status,
        "mergeStateStatus": merge_state_status,
        "review_decision": review_decision,
        "reviewDecision": review_decision,
        "status_check_rollup_state": status_check_rollup_state,
        "statusCheckRollupState": status_check_rollup_state,
        "statusCheckRollup": pr.get("statusCheckRollup").cloned().unwrap_or(Value::Null),
        "active_unresolved_review_threads": active_threads,
        "active_unresolved_review_threads_count": active_threads.len(),
        "review_threads_complete": review_threads_complete,
        "changed_files": changed_files,
        "changed_files_complete": changed_files_complete,
    }))
}

fn active_unresolved_review_threads(pr: &Value) -> Vec<Value> {
    connection_nodes(pr, "reviewThreads")
        .filter(|thread| {
            !thread
                .get("isResolved")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !thread
                    .get("isOutdated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
        .map(|thread| {
            json!({
                "id": value_string(thread.get("id")),
                "path": value_string(thread.get("path")),
                "line": value_u64(thread.get("line")),
                "is_resolved": thread.get("isResolved").and_then(Value::as_bool),
                "is_outdated": thread.get("isOutdated").and_then(Value::as_bool),
                "comments": thread
                    .pointer("/comments/nodes")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            })
        })
        .collect()
}

fn changed_files(pr: &Value) -> Vec<Value> {
    connection_nodes(pr, "files")
        .map(|file| {
            json!({
                "path": value_string(file.get("path")),
                "additions": value_u64(file.get("additions")),
                "deletions": value_u64(file.get("deletions")),
                "change_type": value_string(file.get("changeType")),
                "changeType": value_string(file.get("changeType")),
            })
        })
        .collect()
}

fn connection_nodes<'a>(pr: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    pr.get(field)
        .and_then(|connection| connection.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn connection_has_next_page(pr: &Value, field: &str) -> bool {
    pr.get(field)
        .and_then(|connection| connection.get("pageInfo"))
        .and_then(|page_info| page_info.get("hasNextPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn pr_feedback_signal_for_snapshot(snapshot: &Value) -> ActivitySignal {
    let payload = json!({
        "pr_number": snapshot.get("pr_number").cloned().unwrap_or(Value::Null),
        "pr_url": snapshot.get("pr_url").cloned().unwrap_or(Value::Null),
        "head_oid": snapshot.get("head_oid").cloned().unwrap_or(Value::Null),
        "active_unresolved_review_threads_count": snapshot
            .get("active_unresolved_review_threads_count")
            .cloned()
            .unwrap_or(Value::Null),
        "status_check_rollup_state": snapshot
            .get("status_check_rollup_state")
            .cloned()
            .unwrap_or(Value::Null),
        "merge_state_status": snapshot.get("merge_state_status").cloned().unwrap_or(Value::Null),
        "review_decision": snapshot.get("review_decision").cloned().unwrap_or(Value::Null),
        "is_draft": snapshot.get("is_draft").cloned().unwrap_or(Value::Null),
    });
    ActivitySignal::new(pr_feedback_signal_type(snapshot), payload)
}

fn pr_feedback_signal_type(snapshot: &Value) -> &'static str {
    if snapshot_allows_ready(snapshot) {
        return "PrReadyToMerge";
    }
    if snapshot_review_threads_incomplete(snapshot) {
        return "FeedbackFound";
    }
    if string_eq(snapshot, "review_decision", "CHANGES_REQUESTED") {
        return "ChangesRequested";
    }
    if snapshot
        .get("active_unresolved_review_threads_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 0
    {
        return "FeedbackFound";
    }
    if snapshot_merge_state_requires_repair(snapshot) {
        return "FeedbackFound";
    }
    if snapshot_check_failed(snapshot) {
        return "ChecksFailed";
    }
    "NoFeedbackFound"
}

fn snapshot_allows_ready(snapshot: &Value) -> bool {
    string_eq(snapshot, "status_check_rollup_state", "SUCCESS")
        && string_eq(snapshot, "merge_state_status", "CLEAN")
        && string_eq(snapshot, "review_decision", "APPROVED")
        && snapshot.get("is_draft").and_then(Value::as_bool) == Some(false)
        && snapshot
            .get("active_unresolved_review_threads_count")
            .and_then(Value::as_u64)
            == Some(0)
        && snapshot_review_threads_complete(snapshot)
}

fn snapshot_review_threads_incomplete(snapshot: &Value) -> bool {
    !snapshot_review_threads_complete(snapshot)
}

fn snapshot_review_threads_complete(snapshot: &Value) -> bool {
    snapshot
        .get("review_threads_complete")
        .and_then(Value::as_bool)
        == Some(true)
}

fn snapshot_check_failed(snapshot: &Value) -> bool {
    ["FAILURE", "ERROR"].iter().any(|state| {
        string_eq(snapshot, "status_check_rollup_state", state)
            || string_eq(
                snapshot,
                "status_check_rollup_state",
                &state.to_ascii_lowercase(),
            )
    })
}

fn snapshot_merge_state_requires_repair(snapshot: &Value) -> bool {
    ["DIRTY", "BEHIND"]
        .iter()
        .any(|state| string_eq(snapshot, "merge_state_status", state))
}

fn string_eq(value: &Value, field: &str, expected: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|text| text.eq_ignore_ascii_case(expected))
}

fn pr_feedback_summary(signal_type: &str, snapshot: &Value) -> String {
    let pr_number = snapshot
        .get("pr_number")
        .and_then(Value::as_u64)
        .map(|number| number.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    match signal_type {
        "PrReadyToMerge" => format!("Server-owned PR snapshot shows PR #{pr_number} is ready."),
        "ChangesRequested" => {
            format!("Server-owned PR snapshot shows changes requested for PR #{pr_number}.")
        }
        "FeedbackFound" => {
            if snapshot_review_threads_incomplete(snapshot) {
                format!(
                    "Server-owned PR snapshot could not fully enumerate review threads on PR #{pr_number}."
                )
            } else if snapshot_merge_state_requires_repair(snapshot) {
                format!(
                    "Server-owned PR snapshot found mergeability repair is needed on PR #{pr_number}."
                )
            } else {
                format!("Server-owned PR snapshot found active review feedback on PR #{pr_number}.")
            }
        }
        "ChecksFailed" => {
            format!("Server-owned PR snapshot found failing checks on PR #{pr_number}.")
        }
        _ => format!("Server-owned PR snapshot found no actionable feedback on PR #{pr_number}."),
    }
}

pub(crate) fn value_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn value_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse::<u64>().ok()))
    })
}

pub(crate) fn github_pr_snapshot_failure_result(
    activity: &str,
    target: Option<&GitHubPrSnapshotTarget>,
    error: &anyhow::Error,
) -> ActivityResult {
    ActivityResult::failed(
        activity,
        "Server-owned GitHub PR snapshot collection failed.",
        error.to_string(),
    )
    .with_error_kind(ActivityErrorKind::ExternalDependency)
    .with_artifact(ActivityArtifact::new(
        SERVER_PR_SNAPSHOT_ERROR_ARTIFACT,
        json!({
            "schema": "harness.github.pr_snapshot_error.v1",
            "repo": target.map(|target| target.repo_slug.as_str()),
            "pr_number": target.map(|target| target.pr_number),
            "error": error.to_string(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_pr() -> Value {
        json!({
            "number": 77,
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
        assert_eq!(snapshot["pr_url"], "https://github.com/owner/repo/pull/77");
        assert_eq!(snapshot["head_oid"], "abc123");
        assert_eq!(snapshot["status_check_rollup_state"], "SUCCESS");
        assert_eq!(snapshot["merge_state_status"], "CLEAN");
        assert_eq!(snapshot["review_decision"], "APPROVED");
        assert_eq!(snapshot["is_draft"], false);
        assert_eq!(snapshot["active_unresolved_review_threads_count"], 0);
        assert_eq!(snapshot["changed_files"][0]["path"], "src/lib.rs");
        assert_eq!(snapshot["review_threads_complete"], true);
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

        let result =
            github_pr_snapshot_failure_result("inspect_pr_feedback", Some(&target), &error);

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
}
