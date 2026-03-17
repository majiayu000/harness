//! Output parsers: extract structured data from raw agent output strings.

/// Check if agent output indicates approval (last non-empty line is "APPROVED").
pub fn is_approved(output: &str) -> bool {
    last_non_empty_line(output) == Some("APPROVED")
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

/// Parse `PR_URL=<url>` from agent output (searches from last line).
pub fn parse_pr_url(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("PR_URL=") {
            return Some(url.trim().to_string());
        }
    }
    None
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

/// Check if agent output's last non-empty line is exactly "LGTM".
pub fn is_lgtm(output: &str) -> bool {
    last_non_empty_line(output) == Some("LGTM")
}

/// Check if agent output's last non-empty line is exactly "WAITING".
/// This means the reviewer has not yet re-reviewed after the latest fix commit.
pub fn is_waiting(output: &str) -> bool {
    last_non_empty_line(output) == Some("WAITING")
}

fn last_non_empty_line(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pr_url() {
        let output = "Some output\nPR_URL=https://github.com/owner/repo/pull/42";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/42".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_not_found() {
        assert_eq!(parse_pr_url("no url here"), None);
    }

    #[test]
    fn test_parse_pr_url_with_trailing_whitespace() {
        let output = "done\nPR_URL=https://github.com/o/r/pull/5  \n";
        let url = parse_pr_url(output).unwrap();
        assert_eq!(extract_pr_number(&url), Some(5));
    }

    #[test]
    fn test_extract_pr_number() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42"),
            Some(42)
        );
    }

    #[test]
    fn test_extract_pr_number_invalid() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/"),
            None
        );
    }

    #[test]
    fn test_extract_pr_number_with_path_suffix() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42/files"),
            Some(42)
        );
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42/commits"),
            Some(42)
        );
    }

    #[test]
    fn test_extract_pr_number_with_fragment() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42#discussion_r123"),
            Some(42)
        );
    }

    /// Regression: extract_pr_number must NOT be called on raw multi-line agent
    /// output — the `#` in markdown headers truncates the text before the PR URL.
    /// The correct pipeline is parse_pr_url → extract_pr_number.
    #[test]
    fn test_parse_then_extract_from_markdown_output() {
        let output = "\
I'll implement the feature.

## Plan
1. Read the issue
2. Write the code

### Changes
- Modified `src/lib.rs`

PR_URL=https://github.com/owner/repo/pull/269";

        // Direct call on raw output fails (the bug):
        assert_eq!(extract_pr_number(output), None);

        // Correct two-step pipeline succeeds:
        let url = parse_pr_url(output).expect("should find PR_URL line");
        assert_eq!(extract_pr_number(&url), Some(269));
    }

    #[test]
    fn test_is_lgtm_exact() {
        assert!(is_lgtm("some output\nLGTM"));
        assert!(is_lgtm("LGTM"));
        assert!(is_lgtm("some output\nLGTM\n"));
    }

    #[test]
    fn test_is_lgtm_false_positive() {
        assert!(!is_lgtm("LGTM but actually not done"));
        assert!(!is_lgtm(
            "I think it looks LGTM but needs one more fix\nFIXED"
        ));
        assert!(!is_lgtm("somethingLGTM"));
        assert!(!is_lgtm("notLGTM\n"));
    }

    #[test]
    fn test_is_waiting() {
        assert!(is_waiting("checking reviews...\nWAITING"));
        assert!(is_waiting("WAITING\n"));
        assert!(!is_waiting("LGTM"));
        assert!(!is_waiting("FIXED"));
    }

    #[test]
    fn test_is_approved() {
        assert!(is_approved("looks good\nAPPROVED"));
        assert!(is_approved("APPROVED\n"));
        assert!(is_approved("APPROVED"));
        assert!(!is_approved("APPROVED but with caveats"));
        assert!(!is_approved("ISSUE: something\nAPPROVED not really"));
        assert!(!is_approved("LGTM"));
    }

    #[test]
    fn test_extract_review_issues() {
        let output = "Looking at the diff...\nISSUE: Missing error handling\nThis is fine\nISSUE: Unbounded loop\nAPPROVED";
        let issues = extract_review_issues(output);
        assert_eq!(issues, vec!["Missing error handling", "Unbounded loop"]);
    }

    #[test]
    fn test_extract_review_issues_empty() {
        assert!(extract_review_issues("APPROVED").is_empty());
        assert!(extract_review_issues("ISSUE: ").is_empty());
    }

    #[test]
    fn test_parse_github_pr_url_basic() {
        assert_eq!(
            parse_github_pr_url("https://github.com/owner/repo/pull/42"),
            Some(("owner".to_string(), "repo".to_string(), 42))
        );
    }

    #[test]
    fn test_parse_github_pr_url_with_path_suffix() {
        assert_eq!(
            parse_github_pr_url("https://github.com/owner/repo/pull/42/files"),
            Some(("owner".to_string(), "repo".to_string(), 42))
        );
    }

    #[test]
    fn test_parse_github_pr_url_with_fragment() {
        assert_eq!(
            parse_github_pr_url("https://github.com/owner/repo/pull/42#discussion_r123"),
            Some(("owner".to_string(), "repo".to_string(), 42))
        );
    }

    #[test]
    fn test_parse_github_pr_url_not_a_pr() {
        assert_eq!(
            parse_github_pr_url("https://github.com/owner/repo/issues/42"),
            None
        );
        assert_eq!(
            parse_github_pr_url("https://example.com/owner/repo/pull/42"),
            None
        );
        assert_eq!(parse_github_pr_url("not a url"), None);
    }

    #[test]
    fn test_parse_github_pr_url_invalid_number() {
        assert_eq!(
            parse_github_pr_url("https://github.com/owner/repo/pull/abc"),
            None
        );
    }

    /// Security: malformed reviewer output that contains neither APPROVED nor any ISSUE: line
    /// must not be treated as an approval.
    #[test]
    fn test_malformed_reviewer_output_is_not_approved_and_has_no_issues() {
        let malformed = "I looked at the diff and it seems fine to me.";
        assert!(
            !is_approved(malformed),
            "malformed output without APPROVED must not be treated as approved"
        );
        assert!(
            extract_review_issues(malformed).is_empty(),
            "malformed output without ISSUE: lines should yield an empty issue list"
        );
    }
}
