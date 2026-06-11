use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::{json, Value};
use std::time::Duration;

use crate::github_pr_snapshot::{errors_is_empty, value_string, value_u64};

const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitHubOpenPrHygiene {
    pub repo_slug: String,
    pub pr_number: u64,
    pub pr_url: String,
    pub title: Option<String>,
    pub merge_state_status: Option<String>,
    pub head_oid: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub labels: Vec<String>,
}

impl GitHubOpenPrHygiene {
    pub(crate) fn dirty_age_secs(&self, now: DateTime<Utc>) -> u64 {
        now.signed_duration_since(self.updated_at)
            .to_std()
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    pub(crate) fn requires_mergeability_repair(&self) -> bool {
        self.merge_state_status
            .as_deref()
            .is_some_and(merge_state_requires_repair)
    }

    pub(crate) fn updated_at_rfc3339(&self) -> String {
        self.updated_at.to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubOpenPrHygienePage {
    prs: Vec<GitHubOpenPrHygiene>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

pub(crate) async fn fetch_open_pr_hygiene(
    repo_slug: &str,
    github_token: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<GitHubOpenPrHygiene>> {
    validate_repo_slug(repo_slug)?;
    let client = reqwest::Client::new();
    fetch_open_pr_hygiene_with_client(&client, repo_slug, github_token, limit).await
}

async fn fetch_open_pr_hygiene_with_client(
    client: &reqwest::Client,
    repo_slug: &str,
    github_token: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<GitHubOpenPrHygiene>> {
    let mut prs = Vec::new();
    let mut after = None;
    let limit = limit.max(1);
    while prs.len() < limit {
        let first = (limit - prs.len()).min(100);
        let page =
            fetch_open_pr_hygiene_page(client, repo_slug, github_token, first, after.as_deref())
                .await?;
        prs.extend(page.prs);
        if !page.has_next_page || prs.len() >= limit {
            break;
        }
        after = page.end_cursor;
        if after.is_none() {
            break;
        }
    }
    Ok(prs)
}

async fn fetch_open_pr_hygiene_page(
    client: &reqwest::Client,
    repo_slug: &str,
    github_token: Option<&str>,
    first: usize,
    after: Option<&str>,
) -> anyhow::Result<GitHubOpenPrHygienePage> {
    let (owner, repo) = repo_slug
        .split_once('/')
        .context("validated repo slug should contain owner and repo")?;
    let query = r#"
        query HarnessOpenPrHygiene($owner: String!, $repo: String!, $first: Int!, $after: String) {
          repository(owner: $owner, name: $repo) {
            pullRequests(
              states: OPEN
              first: $first
              after: $after
              orderBy: {field: UPDATED_AT, direction: DESC}
            ) {
              pageInfo {
                hasNextPage
                endCursor
              }
              nodes {
                number
                url
                title
                mergeStateStatus
                headRefOid
                updatedAt
                labels(first: 50) {
                  nodes {
                    name
                  }
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
                "first": i64::try_from(first).unwrap_or(100),
                "after": after,
            }
        }));
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }

    let response = tokio::time::timeout(Duration::from_secs(15), request.send()).await??;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        anyhow::bail!("GitHub PR hygiene query failed with status {status}: {body}");
    }
    let parsed: Value =
        serde_json::from_str(&body).context("GitHub PR hygiene response was invalid JSON")?;
    if let Some(errors) = parsed
        .get("errors")
        .filter(|errors| !errors_is_empty(errors))
    {
        anyhow::bail!("GitHub PR hygiene query returned errors: {errors}");
    }
    let connection = parsed
        .get("data")
        .and_then(|data| data.get("repository").cloned())
        .and_then(|repository| repository.get("pullRequests").cloned())
        .filter(|pull_requests| !pull_requests.is_null())
        .ok_or_else(|| anyhow::anyhow!("GitHub PR hygiene query returned no PR connection"))?;
    parse_open_pr_hygiene_page(repo_slug, &connection)
}

fn parse_open_pr_hygiene_page(
    repo_slug: &str,
    connection: &Value,
) -> anyhow::Result<GitHubOpenPrHygienePage> {
    validate_repo_slug(repo_slug)?;
    let prs = connection
        .get("nodes")
        .and_then(Value::as_array)
        .context("GitHub PR hygiene connection missing nodes")?
        .iter()
        .map(|node| parse_open_pr_hygiene_node(repo_slug, node))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let page_info = connection
        .get("pageInfo")
        .context("GitHub PR hygiene connection missing pageInfo")?;
    Ok(GitHubOpenPrHygienePage {
        prs,
        has_next_page: page_info
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        end_cursor: value_string(page_info.get("endCursor")),
    })
}

fn parse_open_pr_hygiene_node(
    repo_slug: &str,
    node: &Value,
) -> anyhow::Result<GitHubOpenPrHygiene> {
    let pr_number = value_u64(node.get("number")).context("GitHub PR hygiene missing number")?;
    let pr_url = value_string(node.get("url")).context("GitHub PR hygiene missing url")?;
    let updated_at = value_string(node.get("updatedAt"))
        .context("GitHub PR hygiene missing updatedAt")
        .and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|datetime| datetime.with_timezone(&Utc))
                .map_err(|error| anyhow::anyhow!("invalid GitHub PR updatedAt `{value}`: {error}"))
        })?;
    Ok(GitHubOpenPrHygiene {
        repo_slug: repo_slug.to_string(),
        pr_number,
        pr_url,
        title: value_string(node.get("title")),
        merge_state_status: value_string(node.get("mergeStateStatus")),
        head_oid: value_string(node.get("headRefOid")),
        updated_at,
        labels: label_names(node),
    })
}

fn validate_repo_slug(repo_slug: &str) -> anyhow::Result<()> {
    let Some((owner, repo)) = repo_slug.split_once('/') else {
        anyhow::bail!("invalid GitHub repo slug `{repo_slug}`");
    };
    if owner.trim().is_empty() || repo.trim().is_empty() {
        anyhow::bail!("invalid GitHub repo slug `{repo_slug}`");
    }
    Ok(())
}

fn label_names(node: &Value) -> Vec<String> {
    node.pointer("/labels/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| value_string(label.get("name")))
        .collect()
}

fn merge_state_requires_repair(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_uppercase().as_str(),
        "DIRTY" | "BEHIND"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pr_hygiene_parses_open_pr_page() -> anyhow::Result<()> {
        let connection = json!({
            "pageInfo": {"hasNextPage": true, "endCursor": "cursor-1"},
            "nodes": [
                {
                    "number": 77,
                    "url": "https://github.com/owner/repo/pull/77",
                    "title": "Refresh branch",
                    "mergeStateStatus": "DIRTY",
                    "headRefOid": "abc123",
                    "updatedAt": "2026-06-10T00:00:00Z",
                    "labels": {"nodes": [{"name": "ready"}, {"name": "rebase-needed"}]}
                }
            ]
        });

        let page = parse_open_pr_hygiene_page("owner/repo", &connection)?;

        assert!(page.has_next_page);
        assert_eq!(page.end_cursor.as_deref(), Some("cursor-1"));
        assert_eq!(page.prs.len(), 1);
        let pr = &page.prs[0];
        assert_eq!(pr.repo_slug, "owner/repo");
        assert_eq!(pr.pr_number, 77);
        assert_eq!(pr.merge_state_status.as_deref(), Some("DIRTY"));
        assert_eq!(pr.head_oid.as_deref(), Some("abc123"));
        assert!(pr.requires_mergeability_repair());
        assert_eq!(pr.labels, vec!["ready", "rebase-needed"]);
        Ok(())
    }

    #[test]
    fn github_pr_hygiene_uses_updated_at_for_dirty_age() -> anyhow::Result<()> {
        let node = json!({
            "number": 78,
            "url": "https://github.com/owner/repo/pull/78",
            "mergeStateStatus": "BEHIND",
            "headRefOid": "def456",
            "updatedAt": "2026-06-10T00:00:00Z",
            "labels": {"nodes": []}
        });
        let pr = parse_open_pr_hygiene_node("owner/repo", &node)?;
        let now = DateTime::parse_from_rfc3339("2026-06-12T00:00:00Z")?.with_timezone(&Utc);

        assert_eq!(pr.dirty_age_secs(now), 172800);
        assert!(pr.requires_mergeability_repair());
        assert_eq!(pr.updated_at_rfc3339(), "2026-06-10T00:00:00Z");
        Ok(())
    }

    #[test]
    fn github_pr_hygiene_clean_pr_does_not_require_repair() -> anyhow::Result<()> {
        let node = json!({
            "number": 79,
            "url": "https://github.com/owner/repo/pull/79",
            "mergeStateStatus": "CLEAN",
            "updatedAt": "2026-06-10T00:00:00Z"
        });
        let pr = parse_open_pr_hygiene_node("owner/repo", &node)?;

        assert!(!pr.requires_mergeability_repair());
        Ok(())
    }
}
