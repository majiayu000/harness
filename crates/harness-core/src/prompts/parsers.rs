//! Output parser functions for extracting structured data from agent output.

/// Check if agent output indicates approval (last non-empty line is "APPROVED").
pub fn is_approved(output: &str) -> bool {
    last_non_empty_line(output) == Some("APPROVED")
}

/// Check if agent output's last non-empty line is exactly "LGTM".
pub fn is_lgtm(output: &str) -> bool {
    last_non_empty_line(output) == Some("LGTM")
}

/// Check if agent output's last non-empty line is exactly "WAITING".
/// This means Gemini has not yet re-reviewed after the latest fix commit.
pub fn is_waiting(output: &str) -> bool {
    last_non_empty_line(output) == Some("WAITING")
}

/// Extract `ISSUE:` prefixed lines from agent review output.
pub fn extract_review_issues(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("ISSUE:")
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Extract PR number from a GitHub PR URL.
/// Handles URL fragments (`#discussion_r123`) and path suffixes (`/files`, `/commits`).
pub fn extract_pr_number(url: &str) -> Option<u64> {
    let without_fragment = url.split('#').next()?;
    let parts: Vec<&str> = without_fragment.split('/').collect();
    for (i, &part) in parts.iter().enumerate() {
        if (part == "pull" || part == "pulls") && i + 1 < parts.len() {
            if let Ok(n) = parts[i + 1].parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// Parse `PR_URL=<url>` from agent output (searches from last line).
///
/// Only returns URLs matching the strict GitHub PR format
/// `https://github.com/{owner}/{repo}/pull/{number}[/...][#{fragment}]`.
/// This prevents javascript: URI injection (SEC-03) and shell metacharacter
/// injection (e.g. `$(...)` in path segments) when the URL is embedded in
/// reviewer prompts that invoke `gh pr diff`.
///
/// The fragment (if any) is stripped from the returned URL so it cannot
/// escape shell quoting in downstream commands.
pub fn parse_pr_url(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("PR_URL=") {
            let url = url.trim();
            if is_valid_github_pr_url(url) {
                // Strip fragment and trailing slash before returning.
                // The fragment is not needed by `gh pr diff` and a malicious
                // fragment (e.g. `#';cmd;'`) would escape shell quoting when
                // the URL is embedded in reviewer prompts.
                let normalized = url.split('#').next().unwrap_or(url).trim_end_matches('/');
                return Some(normalized.to_string());
            }
        }
    }
    None
}

/// Parse a GitHub PR URL into `(owner, repo, pr_number)`.
///
/// Accepts URLs of the form `https://github.com/{owner}/{repo}/pull/{number}[/...]`.
pub fn parse_github_pr_url(url: &str) -> Option<(String, String, u64)> {
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let parts: Vec<&str> = path.splitn(5, '/').collect();
    if parts.len() >= 4 && parts[2] == "pull" {
        let owner = parts[0].to_string();
        let repo = parts[1].to_string();
        let number_str = parts[3].split('#').next()?;
        let number = number_str.parse::<u64>().ok()?;
        return Some((owner, repo, number));
    }
    None
}

/// Derive `"owner/repo"` slug from a GitHub PR URL, falling back to `"{owner}/{repo}"`.
pub fn repo_slug_from_pr_url(pr_url: Option<&str>) -> String {
    pr_url
        .and_then(parse_github_pr_url)
        .map(|(owner, repo, _)| format!("{owner}/{repo}"))
        .unwrap_or_else(|| "{owner}/{repo}".to_string())
}

/// Extract `ISSUES=N` from agent output (any line). Returns `None` if absent.
pub fn parse_issue_count(output: &str) -> Option<u32> {
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ISSUES=") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

pub(super) fn last_non_empty_line(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
}

/// Returns `true` only for well-formed GitHub PR URLs.
///
/// Accepted:
///   `https://github.com/{owner}/{repo}/pull/{number}[/extra...][#{fragment}]`
///
/// Extra path segments after the PR number (e.g. `/files`, `/commits`) are
/// allowed as long as they contain only safe slug characters.  This avoids
/// silently skipping review for PR URLs that GitHub itself generates with
/// path suffixes.
///
/// Rejected: any other scheme, non-GitHub host, shell metacharacters in any
/// path segment, or non-numeric PR numbers.
fn is_valid_github_pr_url(url: &str) -> bool {
    let rest = match url.strip_prefix("https://github.com/") {
        Some(r) => r,
        None => return false,
    };
    // Strip optional fragment (#discussion_rXXX etc.) before path parsing.
    let path = rest.split_once('#').map_or(rest, |(p, _)| p);
    // Trim trailing slashes so that `/pull/42/` normalises to `/pull/42`.
    let path = path.trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    // Must have at least: owner / repo / "pull" / number
    if parts.len() < 4 {
        return false;
    }
    let is_valid_slug = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    // Validate the mandatory core four segments.
    let core_valid = is_valid_slug(parts[0]) // owner
        && is_valid_slug(parts[1]) // repo
        && parts[2] == "pull"
        && !parts[3].is_empty()
        && parts[3].chars().all(|c| c.is_ascii_digit()); // PR number — digits only
    if !core_valid {
        return false;
    }
    // Any extra path segments (e.g. "files", "commits") must also be safe slugs
    // so that injected shell metacharacters are rejected even in suffix position.
    parts[4..].iter().all(|s| is_valid_slug(s))
}
