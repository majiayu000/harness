use anyhow::Context;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::workflow_runtime_pr_feedback::pr_detection::ClosingPullRequestCandidate;
use harness_workflow::issue_lifecycle::workflow_id;
use harness_workflow::runtime::WorkflowRuntimeStore;

const GITHUB_ISSUE_LINK_QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const GITHUB_ISSUE_LINK_MAX_PAGES: usize = 20;
const GITHUB_ISSUE_LINK_QUERY: &str = r#"
    query HarnessIssueClosingPrs(
      $owner: String!, $repo: String!, $issue: Int!, $cursor: String
    ) {
      repository(owner: $owner, name: $repo) {
        issue(number: $issue) {
          state
          closedByPullRequestsReferences(
            first: 100, after: $cursor, includeClosedPrs: true
          ) {
            pageInfo { hasNextPage endCursor }
            nodes { number url headRefName repository { nameWithOwner } }
          }
        }
      }
    }
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitHubIssueClosingPrs {
    pub(super) issue_state: String,
    pub(super) candidates: Vec<ClosingPullRequestCandidate>,
}

pub(super) async fn fetch_github_issue_closing_prs_with_client(
    client: &reqwest::Client,
    repo_slug: &str,
    issue_number: u64,
    github_token: Option<&str>,
    graphql_url: &str,
) -> anyhow::Result<GitHubIssueClosingPrs> {
    let (owner, repo) = repo_slug
        .split_once('/')
        .filter(|(owner, repo)| !owner.trim().is_empty() && !repo.trim().is_empty())
        .with_context(|| format!("invalid GitHub repo slug `{repo_slug}`"))?;
    let issue_number = i64::try_from(issue_number).context("GitHub issue number exceeds i64")?;
    let mut cursor: Option<String> = None;
    let mut issue_state = None;
    let mut candidates = Vec::new();
    let mut seen_prs = HashSet::new();

    for _ in 0..GITHUB_ISSUE_LINK_MAX_PAGES {
        let page = fetch_github_issue_closing_pr_page(
            client,
            owner,
            repo,
            issue_number,
            cursor.as_deref(),
            github_token,
            graphql_url,
        )
        .await?;
        let issue = page
            .pointer("/data/repository/issue")
            .filter(|issue| !issue.is_null())
            .context("GitHub issue closing-PR query returned no issue data")?;
        let page_state = issue
            .get("state")
            .and_then(Value::as_str)
            .context("GitHub issue closing-PR query omitted issue state")?;
        if let Some(expected) = issue_state.as_deref() {
            anyhow::ensure!(
                expected == page_state,
                "GitHub issue state changed while closing-PR links were paginated"
            );
        } else {
            issue_state = Some(page_state.to_string());
        }
        let connection = issue
            .get("closedByPullRequestsReferences")
            .context("GitHub issue closing-PR query omitted link connection")?;
        for node in connection
            .get("nodes")
            .and_then(Value::as_array)
            .context("GitHub issue closing-PR query omitted link nodes")?
        {
            let number = node
                .get("number")
                .and_then(Value::as_u64)
                .context("GitHub issue closing-PR link omitted PR number")?;
            let repo_slug =
                crate::github_pr_snapshot::value_string(node.pointer("/repository/nameWithOwner"))
                    .context("GitHub issue closing-PR query omitted repository nameWithOwner")?;
            if !seen_prs.insert((repo_slug.clone(), number)) {
                continue;
            }
            candidates.push(ClosingPullRequestCandidate {
                repo_slug,
                number,
                head_ref_name: crate::github_pr_snapshot::value_string(node.get("headRefName"))
                    .context("GitHub issue closing-PR query omitted headRefName")?,
                url: crate::github_pr_snapshot::value_string(node.get("url"))
                    .context("GitHub issue closing-PR query omitted url")?,
            });
        }
        let page_info = connection
            .get("pageInfo")
            .context("GitHub issue closing-PR query omitted pageInfo")?;
        if page_info.get("hasNextPage").and_then(Value::as_bool) != Some(true) {
            return Ok(GitHubIssueClosingPrs {
                issue_state: issue_state.unwrap_or_default(),
                candidates,
            });
        }
        cursor = Some(
            crate::github_pr_snapshot::value_string(page_info.get("endCursor"))
                .context("GitHub issue closing-PR query omitted endCursor")?,
        );
    }

    anyhow::bail!("GitHub issue closing-PR query exceeded {GITHUB_ISSUE_LINK_MAX_PAGES} pages")
}

pub(super) fn closing_pr_belongs_to_repo(
    candidate: &ClosingPullRequestCandidate,
    repo_slug: &str,
) -> bool {
    candidate.repo_slug.eq_ignore_ascii_case(repo_slug)
}

pub(super) async fn recovery_expected_base_ref(
    runtime_store: &WorkflowRuntimeStore,
    project_root: &Path,
    project_id: &str,
    repo: &str,
    issue_number: u64,
) -> anyhow::Result<String> {
    let id = workflow_id(project_id, Some(repo), issue_number);
    if let Some(expected) = runtime_store.get_instance(&id).await?.and_then(|workflow| {
        crate::http::auto_merge::expected_base_ref_from_workflow_data(&workflow.data)
    }) {
        return Ok(expected);
    }
    let expected = harness_core::config::workflow::load_workflow_config(project_root)?
        .base
        .branch;
    let expected = expected.trim();
    anyhow::ensure!(
        !expected.is_empty(),
        "workflow base.branch must not be empty during closing PR recovery"
    );
    Ok(expected.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn fetch_github_issue_closing_pr_page(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    issue_number: i64,
    cursor: Option<&str>,
    github_token: Option<&str>,
    graphql_url: &str,
) -> anyhow::Result<Value> {
    let mut request = client
        .post(graphql_url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "harness-server")
        .json(&json!({
            "query": GITHUB_ISSUE_LINK_QUERY,
            "variables": {
                "owner": owner,
                "repo": repo,
                "issue": issue_number,
                "cursor": cursor,
            }
        }));
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = tokio::time::timeout(GITHUB_ISSUE_LINK_QUERY_TIMEOUT, request.send()).await??;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("GitHub issue closing-PR query failed with status {status}: {body}");
    }
    let parsed: Value =
        serde_json::from_str(&body).context("GitHub issue closing-PR response was invalid JSON")?;
    if parsed
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        anyhow::bail!(
            "GitHub issue closing-PR query returned errors: {}",
            parsed["errors"]
        );
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_pr_query_requests_closed_prs_and_repository_identity() {
        assert!(GITHUB_ISSUE_LINK_QUERY.contains("includeClosedPrs: true"));
        assert!(GITHUB_ISSUE_LINK_QUERY.contains("repository { nameWithOwner }"));
    }

    #[test]
    fn closing_pr_repository_match_is_case_insensitive_and_repo_scoped() {
        let candidate = ClosingPullRequestCandidate {
            repo_slug: "Owner/Repo".to_string(),
            number: 42,
            head_ref_name: "fix/42".to_string(),
            url: "https://github.com/Owner/Repo/pull/42".to_string(),
        };

        assert!(closing_pr_belongs_to_repo(&candidate, "owner/repo"));
        assert!(!closing_pr_belongs_to_repo(&candidate, "owner/other"));
    }
}
