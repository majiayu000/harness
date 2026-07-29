//! Shared GitHub API client helpers.
//!
//! Every server-side GitHub request derives its base URL from
//! `HARNESS_GITHUB_API_BASE_URL` (default `https://api.github.com`) and shares
//! one User-Agent, Accept, API-version, and bearer-auth policy.

/// User-Agent sent with every GitHub API request issued by the server.
pub(crate) const GITHUB_USER_AGENT: &str = "harness-server";

/// GitHub API version pinned on shared requests.
const GITHUB_API_VERSION: &str = "2022-11-28";

/// GitHub API base URL, honoring `HARNESS_GITHUB_API_BASE_URL`.
///
/// Empty values fall back to the public API; a trailing slash is trimmed so
/// callers can append `/repos/...` or `/graphql` directly.
pub(crate) fn github_api_base_url() -> String {
    std::env::var("HARNESS_GITHUB_API_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// GraphQL endpoint derived from the shared base URL.
pub(crate) fn graphql_url() -> String {
    format!("{}/graphql", github_api_base_url())
}

/// Shared `reqwest::Client` for GitHub requests.
pub(crate) fn github_request() -> reqwest::Client {
    reqwest::Client::new()
}

/// Apply the shared GitHub header and auth policy to a request builder.
///
/// Adds `Accept: application/vnd.github+json`, `X-GitHub-Api-Version`, and the
/// shared `User-Agent`, then attaches bearer auth resolved through
/// [`crate::github_auth::resolve_github_token`] when a token is available.
pub(crate) fn apply_github_headers(
    mut request: reqwest::RequestBuilder,
    github_token: Option<&str>,
) -> reqwest::RequestBuilder {
    request = request
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header(reqwest::header::USER_AGENT, GITHUB_USER_AGENT);
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    request
}
