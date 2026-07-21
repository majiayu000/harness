use anyhow::Context;
use chrono::{SecondsFormat, Utc};
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivitySignal, RemoteFactSnapshot,
    PR_FEEDBACK_SNAPSHOT_ARTIFACT, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";
const SERVER_PR_SNAPSHOT_SCHEMA: &str = "harness.github.pr_snapshot.v1";
pub(crate) const GITHUB_PR_SNAPSHOT_ARTIFACT: &str = "github_pr_snapshot";
pub(crate) const SERVER_PR_SNAPSHOT_ERROR_ARTIFACT: &str = "server_pr_snapshot_error";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubPrSnapshotTarget {
    pub repo_slug: String,
    pub pr_number: u64,
    pub expected_base_ref: Option<String>,
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
            expected_base_ref: None,
        })
    }

    pub(crate) fn with_expected_base_ref(mut self, base_ref: impl Into<String>) -> Self {
        let base_ref = base_ref.into();
        let base_ref = base_ref.trim();
        if !base_ref.is_empty() {
            self.expected_base_ref = Some(base_ref.to_string());
        }
        self
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

    pub(crate) fn remote_fact_snapshot(&self) -> anyhow::Result<RemoteFactSnapshot> {
        let repo = value_string(self.normalized_snapshot.get("repo"))
            .context("server PR snapshot missing repo")?;
        let pr_number = value_u64(self.normalized_snapshot.get("pr_number"))
            .context("server PR snapshot missing pr_number")?;
        let state = value_string(self.normalized_snapshot.get("state")).unwrap_or_else(|| {
            pr_readiness_for_snapshot(&self.normalized_snapshot)
                .as_str()
                .to_string()
        });
        let facts_for_hash = stable_pr_fact_hash_input(&self.normalized_snapshot);
        let mut snapshot = RemoteFactSnapshot::new(
            "github",
            repo,
            "pull_request",
            i64::try_from(pr_number)?,
            state,
            facts_for_hash,
            Utc::now(),
        );
        snapshot.facts = self.normalized_snapshot.clone();
        if let Some(url) = value_string(self.normalized_snapshot.get("pr_url")) {
            snapshot = snapshot.with_subject_url(url);
        }
        if let Some(head_sha) = value_string(self.normalized_snapshot.get("head_oid")) {
            snapshot = snapshot.with_head_sha(head_sha);
        }
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrReadiness {
    NeedsFeedbackRepair,
    NeedsCiRepair,
    WaitingForChecks,
    WaitingForMergeability,
    ReadyToMerge,
    Merged,
    ClosedUnmerged,
}

impl PrReadiness {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NeedsFeedbackRepair => "needs_feedback_repair",
            Self::NeedsCiRepair => "needs_ci_repair",
            Self::WaitingForChecks => "waiting_for_checks",
            Self::WaitingForMergeability => "waiting_for_mergeability",
            Self::ReadyToMerge => "ready_to_merge",
            Self::Merged => "merged",
            Self::ClosedUnmerged => "closed_unmerged",
        }
    }
}

pub(crate) async fn fetch_github_pr_snapshot(
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubPrSnapshotArtifacts> {
    let client = reqwest::Client::new();
    fetch_github_pr_snapshot_with_client(&client, target, github_token, &github_graphql_url()).await
}

pub(crate) async fn fetch_github_pr_snapshot_with_client(
    client: &reqwest::Client,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    graphql_url: &str,
) -> anyhow::Result<GitHubPrSnapshotArtifacts> {
    let raw_pr = fetch_github_pr_snapshot_value(client, target, github_token, graphql_url).await?;
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
    graphql_url: &str,
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
              state
              merged
              url
              title
              baseRefName
              headRefName
              headRefOid
              mergeCommit {
                oid
              }
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
              closingIssuesReferences(first: 20) {
                pageInfo {
                  hasNextPage
                  endCursor
                }
                nodes {
                  number
                  url
                }
              }
            }
          }
        }
    "#;

    let mut request = client
        .post(graphql_url)
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

pub(crate) fn github_graphql_url() -> String {
    std::env::var("HARNESS_GITHUB_GRAPHQL_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_GRAPHQL_URL.to_string())
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
    let state = value_string(pr.get("state"));
    let merged = pr.get("merged").and_then(Value::as_bool).or_else(|| {
        state
            .as_deref()
            .map(|state| state.eq_ignore_ascii_case("MERGED"))
    });
    let title = value_string(pr.get("title"));
    let base_ref = value_string(pr.get("baseRefName"));
    let expected_base_ref = target.expected_base_ref.clone();
    let head_ref = value_string(pr.get("headRefName"));
    let head_oid = value_string(pr.get("headRefOid"));
    let merge_commit_sha = pr
        .pointer("/mergeCommit/oid")
        .and_then(|value| value_string(Some(value)));
    let is_draft = pr.get("isDraft").and_then(Value::as_bool);
    let merge_state_status = value_string(pr.get("mergeStateStatus"));
    let review_decision = value_string(pr.get("reviewDecision"));
    let status_check_rollup_state = pr
        .get("statusCheckRollup")
        .and_then(|rollup| rollup.get("state"))
        .and_then(|value| value_string(Some(value)));
    let active_threads = active_unresolved_review_threads(pr);
    let changed_files = changed_files(pr);
    let closing_issues = closing_issues(pr);
    let observed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let review_threads_complete = !connection_has_next_page(pr, "reviewThreads");
    let changed_files_complete = !connection_has_next_page(pr, "files");
    let closing_issues_complete = !connection_has_next_page(pr, "closingIssuesReferences");

    Ok(json!({
        "schema": SERVER_PR_SNAPSHOT_SCHEMA,
        "snapshot_source": "server_github_graphql",
        "observed_at": observed_at,
        "repo": target.repo_slug,
        "pr_number": pr_number,
        "state": state,
        "merged": merged,
        "pr_url": pr_url,
        "url": pr_url,
        "title": title,
        "base_ref": base_ref,
        "baseRefName": base_ref,
        "expected_base_ref": expected_base_ref,
        "head_ref": head_ref,
        "headRefName": head_ref,
        "head_oid": head_oid,
        "headRefOid": head_oid,
        "merge_commit_sha": merge_commit_sha,
        "mergeCommitOid": merge_commit_sha,
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
        "closing_issues": closing_issues,
        "closing_issues_complete": closing_issues_complete,
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

fn closing_issues(pr: &Value) -> Vec<Value> {
    connection_nodes(pr, "closingIssuesReferences")
        .map(|issue| {
            json!({
                "number": value_u64(issue.get("number")),
                "url": value_string(issue.get("url")),
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
    if !snapshot_base_ref_matches_expected(snapshot) {
        return "FeedbackFound";
    }
    if snapshot_check_failed(snapshot) {
        return "ChecksFailed";
    }
    "NoFeedbackFound"
}

pub(crate) fn pr_readiness_for_snapshot(snapshot: &Value) -> PrReadiness {
    if string_eq(snapshot, "state", "MERGED") {
        return PrReadiness::Merged;
    }
    if string_eq(snapshot, "state", "CLOSED") {
        return PrReadiness::ClosedUnmerged;
    }
    if snapshot_allows_ready(snapshot) {
        return PrReadiness::ReadyToMerge;
    }
    if snapshot_check_failed(snapshot) {
        return PrReadiness::NeedsCiRepair;
    }
    if !string_eq(snapshot, "status_check_rollup_state", "SUCCESS") {
        return PrReadiness::WaitingForChecks;
    }
    if snapshot_review_threads_incomplete(snapshot)
        || string_eq(snapshot, "review_decision", "CHANGES_REQUESTED")
        || snapshot
            .get("active_unresolved_review_threads_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
        || snapshot_merge_state_requires_repair(snapshot)
    {
        return PrReadiness::NeedsFeedbackRepair;
    }
    if !string_eq(snapshot, "merge_state_status", "CLEAN") {
        return PrReadiness::WaitingForMergeability;
    }
    PrReadiness::NeedsFeedbackRepair
}

fn stable_pr_fact_hash_input(snapshot: &Value) -> Value {
    let mut stable = snapshot.clone();
    if let Some(object) = stable.as_object_mut() {
        object.remove("observed_at");
    }
    stable
}

fn snapshot_allows_ready(snapshot: &Value) -> bool {
    string_eq(snapshot, "status_check_rollup_state", "SUCCESS")
        && string_eq(snapshot, "merge_state_status", "CLEAN")
        && snapshot_base_ref_matches_expected(snapshot)
        && string_eq(snapshot, "review_decision", "APPROVED")
        && snapshot.get("is_draft").and_then(Value::as_bool) == Some(false)
        && snapshot
            .get("active_unresolved_review_threads_count")
            .and_then(Value::as_u64)
            == Some(0)
        && snapshot_review_threads_complete(snapshot)
}

fn snapshot_base_ref_matches_expected(snapshot: &Value) -> bool {
    let Some(expected_base_ref) = value_string(snapshot.get("expected_base_ref")) else {
        return true;
    };
    value_string(snapshot.get("base_ref")).is_some_and(|base_ref| base_ref == expected_base_ref)
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
            } else if !snapshot_base_ref_matches_expected(snapshot) {
                format!(
                    "Server-owned PR snapshot found PR #{pr_number} targets the wrong base branch."
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
#[path = "github_pr_snapshot_tests.rs"]
mod tests;
