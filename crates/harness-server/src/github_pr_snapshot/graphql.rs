use super::{errors_is_empty, value_string, GitHubPrSnapshotTarget};
use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};

pub(super) const GITHUB_PR_SNAPSHOT_QUERY: &str = r#"
    query HarnessPrSnapshot($owner: String!, $repo: String!, $pr: Int!) {
      repository(owner: $owner, name: $repo) {
        pullRequest(number: $pr) {
          number
          state
          merged
          url
          title
          updatedAt
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
            id
            state
            contexts(first: 100) {
              pageInfo {
                hasNextPage
                endCursor
              }
              nodes {
                __typename
                ... on CheckRun {
                  id
                  databaseId
                  name
                  status
                  conclusion
                  detailsUrl
                }
                ... on StatusContext {
                  id
                  context
                  state
                  targetUrl
                }
              }
            }
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

const GITHUB_PR_CHECK_CONTEXTS_QUERY: &str = r#"
    query HarnessPrCheckContexts($rollup: ID!, $after: String!) {
      node(id: $rollup) {
        ... on StatusCheckRollup {
          contexts(first: 100, after: $after) {
            pageInfo {
              hasNextPage
              endCursor
            }
            nodes {
              __typename
              ... on CheckRun {
                id
                databaseId
                name
                status
                conclusion
                detailsUrl
              }
              ... on StatusContext {
                id
                context
                state
                targetUrl
              }
            }
          }
        }
      }
    }
"#;

const GITHUB_PR_CHECK_CONTEXT_MAX_ADDITIONAL_PAGES: usize = 3;

#[derive(Debug, Deserialize)]
struct GitHubPrSnapshotGraphQlResponse {
    data: Option<Value>,
    errors: Option<Value>,
}

pub(super) async fn fetch_github_pr_snapshot_value(
    client: &reqwest::Client,
    target: &GitHubPrSnapshotTarget,
    github_token: Option<&str>,
    graphql_url: &str,
) -> anyhow::Result<Value> {
    let (owner, repo) = target
        .repo_slug
        .split_once('/')
        .context("validated repo slug should contain owner and repo")?;
    let data = execute_github_pr_graphql(
        client,
        github_token,
        graphql_url,
        GITHUB_PR_SNAPSHOT_QUERY,
        json!({
            "owner": owner,
            "repo": repo,
            "pr": target.pr_number as i64,
        }),
    )
    .await?;
    let mut pr = data
        .get("repository")
        .and_then(|repository| repository.get("pullRequest"))
        .cloned()
        .filter(|pr| !pr.is_null())
        .ok_or_else(|| anyhow::anyhow!("GitHub PR snapshot query returned no PR data"))?;
    fetch_remaining_status_check_contexts(client, github_token, graphql_url, &mut pr).await?;
    Ok(pr)
}

async fn execute_github_pr_graphql(
    client: &reqwest::Client,
    github_token: Option<&str>,
    graphql_url: &str,
    query: &str,
    variables: Value,
) -> anyhow::Result<Value> {
    let request = crate::github_client::apply_github_headers(
        client.post(graphql_url).json(&json!({
            "query": query,
            "variables": variables,
        })),
        github_token,
    );
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
        .ok_or_else(|| anyhow::anyhow!("GitHub PR snapshot query returned no data"))
}

async fn fetch_remaining_status_check_contexts(
    client: &reqwest::Client,
    github_token: Option<&str>,
    graphql_url: &str,
    pr: &mut Value,
) -> anyhow::Result<()> {
    if !has_more_status_check_contexts(pr) {
        return Ok(());
    }
    let rollup_id = pr
        .pointer("/statusCheckRollup/id")
        .and_then(|value| value_string(Some(value)))
        .context("GitHub PR snapshot paginated check contexts are missing rollup id")?;
    let mut seen_cursors = HashSet::new();

    for _ in 0..GITHUB_PR_CHECK_CONTEXT_MAX_ADDITIONAL_PAGES {
        if !has_more_status_check_contexts(pr) {
            break;
        }
        let cursor = pr
            .pointer("/statusCheckRollup/contexts/pageInfo/endCursor")
            .and_then(|value| value_string(Some(value)))
            .context("GitHub PR snapshot paginated check contexts are missing end cursor")?;
        if !seen_cursors.insert(cursor.clone()) {
            anyhow::bail!("GitHub PR snapshot check context pagination repeated cursor `{cursor}`");
        }
        let data = execute_github_pr_graphql(
            client,
            github_token,
            graphql_url,
            GITHUB_PR_CHECK_CONTEXTS_QUERY,
            json!({
                "rollup": rollup_id,
                "after": cursor,
            }),
        )
        .await?;
        append_status_check_context_page(pr, &data)?;
    }
    Ok(())
}

fn has_more_status_check_contexts(pr: &Value) -> bool {
    pr.pointer("/statusCheckRollup/contexts/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn append_status_check_context_page(pr: &mut Value, data: &Value) -> anyhow::Result<()> {
    let page = data
        .pointer("/node/contexts")
        .context("GitHub PR snapshot check context pagination returned no page")?;
    let page_nodes = page
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let page_info = page
        .get("pageInfo")
        .cloned()
        .context("GitHub PR snapshot check context pagination returned no page info")?;
    let contexts = pr
        .pointer_mut("/statusCheckRollup/contexts")
        .and_then(Value::as_object_mut)
        .context("GitHub PR snapshot check context connection is invalid")?;
    contexts
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("GitHub PR snapshot check context nodes are invalid")?
        .extend(page_nodes);
    contexts.insert("pageInfo".to_string(), page_info);
    Ok(())
}
