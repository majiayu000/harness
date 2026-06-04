use harness_core::prompts;
use reqwest::header::{ACCEPT, LINK, RETRY_AFTER, USER_AGENT};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const GITHUB_API_BASE_URL: &str = "https://api.github.com";
const GITHUB_PR_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const GITHUB_PR_LOOKUP_MAX_PAGES: usize = 20;
const GITHUB_ERROR_BODY_SNIPPET_CHARS: usize = 300;

#[derive(Debug, Deserialize)]
struct GhPrListItem {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    /// Full PR URL, e.g. `https://github.com/owner/repo/pull/42`.
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct GitHubPullItem {
    number: u64,
    html_url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    head: GitHubPullHead,
}

#[derive(Debug, Deserialize)]
struct GitHubPullHead {
    #[serde(rename = "ref")]
    ref_name: String,
}

impl From<GitHubPullItem> for GhPrListItem {
    fn from(value: GitHubPullItem) -> Self {
        Self {
            number: value.number,
            head_ref_name: value.head.ref_name,
            url: value.html_url,
            title: value.title,
            body: value.body.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HarnessMentionCommand {
    Mention,
    Review,
    FixCi,
}

/// Parse the first `@harness` mention found while scanning line-by-line.
/// For each line, only the first `@harness` occurrence is considered.
pub(crate) fn parse_harness_mention_command(body: &str) -> Option<HarnessMentionCommand> {
    for line in body.lines() {
        let lowercase = line.trim().to_ascii_lowercase();
        if let Some(idx) = lowercase.find("@harness") {
            let mut command = lowercase[idx + "@harness".len()..].trim_start();
            command = command.trim_start_matches(|ch: char| {
                ch.is_whitespace() || ch == ':' || ch == ',' || ch == '-' || ch == '.'
            });

            if command.starts_with("fix ci")
                || command.starts_with("fix-ci")
                || command.starts_with("fix_ci")
            {
                return Some(HarnessMentionCommand::FixCi);
            }
            if command.starts_with("review") {
                return Some(HarnessMentionCommand::Review);
            }
            return Some(HarnessMentionCommand::Mention);
        }
    }

    None
}

pub(crate) struct PromptBuilder {
    title: String,
    sections: Vec<(String, String)>,
}

impl PromptBuilder {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            sections: Vec::new(),
        }
    }

    /// Add a named section with `content` wrapped in external_data tags.
    pub(crate) fn add_section(mut self, name: &str, content: &str) -> Self {
        self.sections
            .push((name.to_string(), prompts::wrap_external_data(content)));
        self
    }

    /// Add an optional URL metadata line. No-op if `url` is `None`.
    pub(crate) fn add_optional_url(mut self, label: &str, url: Option<&str>) -> Self {
        if let Some(u) = url {
            let safe = prompts::wrap_external_data(u);
            self.sections
                .push((String::new(), format!("- {label}: {safe}")));
        }
        self
    }

    /// Assemble the prompt: title, then each section, with a trailing newline.
    pub(crate) fn build(self) -> String {
        let mut out = self.title;
        for (name, content) in &self.sections {
            out.push('\n');
            if name.is_empty() {
                out.push_str(content);
            } else {
                out.push_str(name);
                out.push_str(":\n");
                out.push_str(content);
            }
        }
        out.push('\n');
        out
    }
}

pub(crate) fn build_fix_ci_prompt(
    repository: &str,
    pr_number: u64,
    comment_body: &str,
    comment_url: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");
    let preamble = PromptBuilder::new(format!(
        "CI failure repair requested for PR #{pr_number} in `{repository}`."
    ))
    .add_optional_url("Trigger comment", comment_url)
    .add_optional_url("PR URL", pr_url)
    .add_section("Command payload", comment_body)
    .build();

    format!(
        "{preamble}\n\
         Required workflow:\n\
         1. Inspect failing checks for PR #{pr_number} (`gh pr checks {pr_number}`)\n\
         2. Investigate CI failure details from logs and failing tests\n\
         3. Implement a minimal fix that makes CI green\n\
         4. Run the repository's standard validation commands for the affected changes (including all failing/required CI checks)\n\
         5. Commit and push to the existing PR branch\n\n\
         On the last line, print PR_URL={canonical_pr_url}"
    )
}

pub(crate) fn build_pr_approved_prompt(
    repository: &str,
    pr_number: u64,
    review_url: Option<&str>,
) -> String {
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");
    let preamble = PromptBuilder::new(format!(
        "PR #{pr_number} in `{repository}` has been approved by a reviewer."
    ))
    .add_optional_url("Review URL", review_url)
    .build();

    format!(
        "{preamble}\n\
         Action required:\n\
         Post a comment on the PR indicating it is ready to merge:\n\
         gh pr comment {pr_number} --repo {repository} --body \"Approved — ready to merge.\"\n\n\
         Then stop. There is nothing else to implement.\n\n\
         On the last line, print PR_URL={canonical_pr_url}"
    )
}

/// Query GitHub for an existing open PR that *claims to close* the given issue.
/// Returns `(pr_number, branch_name, pr_url)` if found.
///
/// `gh pr list --search "#N"` is a free-text search: it returns any PR whose
/// title, body, or comments merely mention `#N`. That led to cross-issue
/// pollution when one PR's body referenced another issue number as context
/// (e.g. PR for #794 that says "depends on #791 being fixed first") and a
/// later task for #791 then tried to "continue" on that PR's branch.
///
/// This function now filters results so only PRs that **explicitly declare a
/// closing relationship** to the issue are returned via a closing keyword in
/// title or body: `closes|closed|close|fixes|fixed|fix|resolves|resolved|resolve #N`.
pub(crate) async fn find_existing_pr_for_issue_with_token(
    project: &Path,
    issue: u64,
    github_token: Option<&str>,
) -> anyhow::Result<Option<(u64, String, String)>> {
    let Some(repo_slug) = detect_repo_slug(project).await else {
        tracing::debug!(
            issue,
            project = %project.display(),
            "existing PR lookup skipped because repository slug is unavailable"
        );
        return Ok(None);
    };

    let client = reqwest::Client::new();
    find_existing_pr_for_issue_in_repo(
        &client,
        &repo_slug,
        issue,
        github_token,
        GITHUB_API_BASE_URL,
    )
    .await
}

async fn find_existing_pr_for_issue_in_repo(
    client: &reqwest::Client,
    repo_slug: &str,
    issue: u64,
    github_token: Option<&str>,
    api_base_url: &str,
) -> anyhow::Result<Option<(u64, String, String)>> {
    let mut next_url = Some(github_pulls_url(api_base_url, repo_slug));
    let mut seen_urls = HashSet::new();
    let mut page_count = 0usize;

    while let Some(url) = next_url {
        if page_count >= GITHUB_PR_LOOKUP_MAX_PAGES {
            tracing::warn!(
                issue,
                repo = %repo_slug,
                page_limit = GITHUB_PR_LOOKUP_MAX_PAGES,
                "existing PR lookup stopped after reaching the GitHub pagination page limit"
            );
            break;
        }
        page_count += 1;

        if !seen_urls.insert(url.clone()) {
            tracing::debug!(
                issue,
                repo = %repo_slug,
                url = %url,
                "existing PR lookup stopped because GitHub pagination repeated a URL"
            );
            break;
        }

        let page = fetch_github_pr_page(client, &url, repo_slug, issue, github_token).await?;

        if let Some(item) = page
            .items
            .into_iter()
            .find(|item| pr_claims_to_close_issue(item, issue, Some(repo_slug)))
        {
            return Ok(Some((item.number, item.head_ref_name, item.url)));
        }

        next_url = page.next_url;
    }

    Ok(None)
}

fn github_pulls_url(api_base_url: &str, repo_slug: &str) -> String {
    format!(
        "{}/repos/{repo_slug}/pulls?state=open&per_page=100",
        api_base_url.trim_end_matches('/')
    )
}

struct GitHubPrPage {
    items: Vec<GhPrListItem>,
    next_url: Option<String>,
}

async fn fetch_github_pr_page(
    client: &reqwest::Client,
    url: &str,
    repo_slug: &str,
    issue: u64,
    github_token: Option<&str>,
) -> anyhow::Result<GitHubPrPage> {
    let mut request = client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, "harness-server");
    if let Some(token) = crate::github_auth::resolve_github_token(github_token) {
        request = request.bearer_auth(token);
    }
    let response = match tokio::time::timeout(GITHUB_PR_LOOKUP_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            anyhow::bail!(
                "failed to fetch GitHub pull request page for {repo_slug} issue #{issue} at {url}: {e}"
            );
        }
        Err(_) => {
            anyhow::bail!(
                "timed out fetching GitHub pull request page for {repo_slug} issue #{issue} at {url}"
            );
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let rate_limit_remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let is_rate_limited = status.as_u16() == 429
            || retry_after.is_some()
            || (status.as_u16() == 403 && rate_limit_remaining.as_deref() == Some("0"));
        let body = response
            .text()
            .await
            .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
        let body = error_body_snippet(&body);
        let rate_limit_context = if is_rate_limited { " rate limit" } else { "" };
        let retry_after = retry_after.as_deref().unwrap_or("none");
        let rate_limit_remaining = rate_limit_remaining.as_deref().unwrap_or("unknown");
        anyhow::bail!(
            "GitHub pull request lookup for {repo_slug} issue #{issue} at {url} returned {status};{rate_limit_context} retry-after={retry_after}; x-ratelimit-remaining={rate_limit_remaining}; body={body}"
        );
    }
    let next_url = next_link_from_headers(response.headers());
    let items: Vec<GhPrListItem> = response
        .json::<Vec<GitHubPullItem>>()
        .await
        .map(|items| items.into_iter().map(Into::into).collect())
        .map_err(|e| anyhow::anyhow!("invalid GitHub pull request response: {e}"))?;

    Ok(GitHubPrPage { items, next_url })
}

fn error_body_snippet(body: &str) -> String {
    let mut chars = body.chars();
    let snippet: String = chars
        .by_ref()
        .take(GITHUB_ERROR_BODY_SNIPPET_CHARS)
        .collect();
    if chars.next().is_some() {
        format!("{snippet}...")
    } else {
        snippet
    }
}

fn next_link_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(LINK)?.to_str().ok()?;
    parse_next_link(link)
}

fn parse_next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let mut segments = part.split(';').map(str::trim);
        let url_segment = segments.next()?;
        if !url_segment.starts_with('<') || !url_segment.ends_with('>') {
            return None;
        }
        if segments.any(link_segment_has_next_rel) {
            Some(url_segment[1..url_segment.len() - 1].to_string())
        } else {
            None
        }
    })
}

fn link_segment_has_next_rel(segment: &str) -> bool {
    let Some((key, value)) = segment.split_once('=') else {
        return false;
    };
    if !key.trim().eq_ignore_ascii_case("rel") {
        return false;
    }
    value
        .trim()
        .trim_matches('"')
        .split_ascii_whitespace()
        .any(|rel| rel.eq_ignore_ascii_case("next"))
}

/// Return true iff `item` declares a closing relationship to `issue` — i.e.
/// this PR, when merged, should close the issue.
///
/// A plain mention of `#N` in the body (e.g. "related to #N", "depends on #N")
/// is NOT sufficient and must return false, otherwise one issue's PR can be
/// silently reused for a different issue.
///
/// The `(#N)` trailing-title suffix is intentionally NOT treated as a close
/// signal here: PR creation prompts do not contractually enforce that suffix,
/// so any manually-created or unrelated PR whose title happens to end with
/// `(#N)` (e.g. `chore: cleanup scheduler (#799)`) would be falsely matched.
/// Only GitHub's closing-keyword syntax is reliable.
///
/// `repo_slug` is the `owner/repo` identifier for the current repository.
/// When provided, repo-qualified references (e.g. `Fixes owner/repo#N`) are
/// only accepted if the qualifier matches `repo_slug`, preventing cross-repo
/// false positives. When `None` the qualifier is accepted as-is.
fn pr_claims_to_close_issue(item: &GhPrListItem, issue: u64, repo_slug: Option<&str>) -> bool {
    let title = item.title.to_ascii_lowercase();
    let body = item.body.to_ascii_lowercase();
    // Scan title and body independently so that a keyword at the end of the
    // title cannot be paired with `#N` at the start of the body.
    field_claims_to_close_issue(&title, issue, repo_slug)
        || field_claims_to_close_issue(&body, issue, repo_slug)
}

fn field_claims_to_close_issue(field: &str, issue: u64, repo_slug: Option<&str>) -> bool {
    // GitHub's auto-close keywords (case-insensitive per GitHub docs).
    // See: https://docs.github.com/en/get-started/writing-on-github/working-with-advanced-formatting/using-keywords-in-issues-and-pull-requests
    // Word boundary on the left prevents e.g. "prefixes #5" from matching "fixes #5".
    const CLOSE_KEYWORDS: &[&str] = &[
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let needle = format!("#{issue}");
    // Scan each occurrence of `#N` and check:
    //   1. the preceding token is a close keyword, AND
    //   2. the matched `#N` is not itself a prefix of a longer issue reference
    //      (e.g. scanning for `#79` must not match `#791` or `#791abc`).
    for (idx, _) in field.match_indices(needle.as_str()) {
        let after = &field[idx + needle.len()..];
        if after.chars().next().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let prefix = &field[..idx];
        // Skip trailing whitespace/punctuation between keyword and `#N`.
        let trimmed = prefix.trim_end_matches(|c: char| c.is_whitespace() || c == ':');
        if CLOSE_KEYWORDS
            .iter()
            .any(|kw| trimmed.ends_with(*kw) && word_boundary_before(trimmed, kw.len()))
        {
            return true;
        }
        // Also handle GitHub's repo-qualified syntax: "Fixes owner/repo#N".
        // Strip the trailing "owner/repo" slug and check the remainder for a keyword.
        if let Some(remainder) = strip_trailing_repo_qualifier(trimmed) {
            // Reject references to a different repository.
            if let Some(slug) = repo_slug {
                let qualifier = trimmed[remainder.len()..].trim();
                if !qualifier.eq_ignore_ascii_case(slug) {
                    continue;
                }
            }
            let inner = remainder.trim_end_matches(|c: char| c.is_whitespace() || c == ':');
            if CLOSE_KEYWORDS
                .iter()
                .any(|kw| inner.ends_with(*kw) && word_boundary_before(inner, kw.len()))
            {
                return true;
            }
        }
    }
    false
}

/// Check whether the character just before a trailing substring of length `kw_len`
/// is a word boundary (non-alphanumeric), so that "prefix-fix" does not match
/// the keyword "fix".
fn word_boundary_before(s: &str, kw_len: usize) -> bool {
    let start = s.len().saturating_sub(kw_len);
    if start == 0 {
        return true;
    }
    // Use char-aware look-back so that multi-byte UTF-8 sequences are handled
    // correctly. A raw-byte check would treat the trailing byte of a multi-byte
    // character (e.g. `é` → 0xC3 0xA9) as non-ASCII and therefore non-
    // alphanumeric, falsely reporting a word boundary for inputs like
    // `éfixes #791`.
    s[..start]
        .chars()
        .next_back()
        .map(|c| !c.is_alphanumeric())
        .unwrap_or(true)
}

/// If `s` ends with an `owner/repo` slug pattern, return the portion before the slug.
/// Used to detect repo-qualified close references like `fixes owner/repo#N`.
fn strip_trailing_repo_qualifier(s: &str) -> Option<&str> {
    let slash_pos = s.rfind('/')?;
    let repo_part = &s[slash_pos + 1..];
    if repo_part.is_empty()
        || !repo_part
            .chars()
            .all(|c: char| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }
    let before_slash = &s[..slash_pos];
    // Find the start of the owner segment: last non-slug character before the slash.
    let owner_start = before_slash
        .rfind(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .map(|i| i + 1)
        .unwrap_or(0);
    if owner_start >= slash_pos {
        return None;
    }
    Some(&s[..owner_start])
}

/// Parse `"owner/repo"` from a git remote URL.
///
/// Handles HTTPS (`https://github.com/owner/repo.git`),
/// SCP-style SSH (`git@github.com:owner/repo.git`), and
/// ssh-scheme SSH (`ssh://git@github.com/owner/repo.git`) formats.
pub(crate) fn parse_repo_slug_from_remote_url(url: &str) -> Option<String> {
    // SCP-style SSH: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let slug = rest.trim_end_matches(".git");
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }
    // ssh-scheme SSH: ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        let slug = rest.trim_end_matches(".git");
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }
    // HTTPS: https://github.com/owner/repo.git
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let slug = rest.trim_end_matches(".git");
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }
    None
}

/// Detect the `"owner/repo"` slug by reading configured git remotes from
/// `.git/config`.
///
/// This intentionally avoids launching `git`. It prefers `origin` for
/// stability but falls back to any other GitHub remote, which keeps the
/// cross-repo guard active in repositories whose primary remote has a
/// different name.
pub(crate) async fn detect_repo_slug(project: &Path) -> Option<String> {
    let mut remotes = Vec::new();
    for config_path in git_config_candidates(project) {
        let Ok(config) = std::fs::read_to_string(&config_path) else {
            continue;
        };
        remotes.extend(parse_remote_urls_from_git_config(&config));
    }

    let mut fallback: Option<String> = None;
    for (name, url) in remotes {
        if let Some(slug) = parse_repo_slug_from_remote_url(&url) {
            if name == "origin" {
                return Some(slug);
            }
            if fallback.is_none() {
                fallback = Some(slug);
            }
        }
    }
    fallback
}

fn git_config_candidates(project: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut current = if project.is_dir() {
        Some(project)
    } else {
        project.parent()
    };
    while let Some(dir) = current {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            candidates.push(dotgit.join("config"));
            break;
        }
        if dotgit.is_file() {
            candidates.extend(config_candidates_from_gitdir_file(&dotgit));
            break;
        }
        current = dir.parent();
    }
    candidates
}

fn config_candidates_from_gitdir_file(dotgit: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(dotgit) else {
        return Vec::new();
    };
    let Some(raw_gitdir) = contents.trim().strip_prefix("gitdir:") else {
        return Vec::new();
    };
    let gitdir = {
        let path = PathBuf::from(raw_gitdir.trim());
        if path.is_absolute() {
            path
        } else {
            dotgit
                .parent()
                .map(|parent| parent.join(&path))
                .unwrap_or(path)
        }
    };

    let mut candidates = vec![gitdir.join("config")];
    let commondir = gitdir.join("commondir");
    if let Ok(raw_common) = std::fs::read_to_string(&commondir) {
        let common_path = PathBuf::from(raw_common.trim());
        let common_path = if common_path.is_absolute() {
            common_path
        } else {
            gitdir.join(common_path)
        };
        candidates.push(common_path.join("config"));
    }
    if let Some(common_git_dir) = gitdir.parent().and_then(|p| p.parent()) {
        candidates.push(common_git_dir.join("config"));
    }
    candidates
}

fn parse_remote_urls_from_git_config(config: &str) -> Vec<(String, String)> {
    let mut current_remote: Option<String> = None;
    let mut remotes = Vec::new();
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = &trimmed[1..trimmed.len() - 1];
            current_remote = parse_remote_section_name(section);
            continue;
        }
        let Some(name) = current_remote.as_deref() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            let url = trim_git_config_value(value);
            if !url.is_empty() {
                remotes.push((name.to_string(), url.to_string()));
            }
        }
    }
    remotes
}

fn parse_remote_section_name(section: &str) -> Option<String> {
    let section = section.trim();
    if let Some(name) = section.strip_prefix("remote.") {
        let name = name.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }

    let rest = section.strip_prefix("remote")?;
    if rest.chars().next().is_some_and(|ch| !ch.is_whitespace()) {
        return None;
    }
    let rest = rest.trim_start();
    let quoted = rest.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let name = quoted[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn trim_git_config_value(value: &str) -> &str {
    let value = value.trim();
    for (idx, ch) in value.char_indices() {
        if (ch == '#' || ch == ';')
            && value[..idx]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            return value[..idx].trim_end();
        }
    }
    value
}

#[cfg(test)]
#[path = "pr_detection_tests.rs"]
mod tests;
