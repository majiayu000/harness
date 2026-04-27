use super::last_non_empty_line;

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

/// Strip ANSI CSI escape sequences (e.g. colour codes) from `s`.
///
/// Handles `ESC [ <params> <letter>` sequences. Bare ESC characters not
/// followed by `[` are consumed silently. Uses a state machine so no regex
/// dependency is required.
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                              // Consume digits and semicolons until an ASCII letter ends the sequence.
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            // else: bare ESC — skip it
        } else {
            result.push(c);
        }
    }
    result
}

/// Scan `output` for the last valid bare GitHub PR URL.
///
/// Used as a fallback when no `PR_URL=` sentinel is found. Each candidate is
/// validated with `is_valid_github_pr_url` to maintain the same injection
/// protections. Returns the last valid URL found (consistent with the sentinel
/// scan which also takes the last occurrence).
fn scan_bare_pr_url(output: &str) -> Option<String> {
    let needle = "https://github.com/";
    let mut last: Option<String> = None;
    for line in output.lines() {
        let mut search_from = 0usize;
        while let Some(rel) = line[search_from..].find(needle) {
            let url_start = search_from + rel;
            let candidate = &line[url_start..];
            let url_end = candidate
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
                .unwrap_or(candidate.len());
            // Trim trailing sentence punctuation that cannot be part of a PR URL.
            // Example: "see https://github.com/o/r/pull/5." — the period ends the sentence.
            let url = candidate[..url_end]
                .trim_end_matches(|c: char| matches!(c, '.' | ',' | '!' | '?' | ')' | ']'));
            if is_valid_github_pr_url(url) {
                let normalized = url.split('#').next().unwrap_or(url).trim_end_matches('/');
                last = Some(normalized.to_string());
            }
            search_from = url_start + url_end.max(1);
        }
    }
    last
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
///
/// ANSI colour codes are stripped before scanning so that terminals that
/// decorate the output do not break the literal `PR_URL=` match. If no
/// `PR_URL=` sentinel is found, a second pass searches for any bare GitHub
/// PR URL in the output and returns the last valid match.
pub fn parse_pr_url(output: &str) -> Option<String> {
    let stripped = strip_ansi_codes(output);

    // First pass: explicit PR_URL= sentinel (scan from end to get the last one).
    for line in stripped.lines().rev() {
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

    // Second pass: bare GitHub PR URL anywhere in the output.
    scan_bare_pr_url(&stripped)
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

/// Parse the created GitHub issue number from agent output.
///
/// Only recognises the explicit authoritative sentinel line:
///   `CREATED_ISSUE=<number>`
///
/// URL scanning was deliberately removed: any unrelated issue link the agent
/// mentions (in summaries, tool output, or PR descriptions) would be a false
/// positive, poisoning `external_id` and causing the dedup layer to suppress
/// the wrong webhook.  The auto-fix prompt explicitly instructs the agent to
/// emit this sentinel, so heuristic URL extraction is unnecessary.
///
/// Returns the **last** sentinel value found (the most likely final answer
/// when the agent self-corrects mid-output).
pub fn parse_created_issue_number(output: &str) -> Option<u64> {
    let mut last: Option<u64> = None;

    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("CREATED_ISSUE=") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                last = Some(n);
            }
        }
    }

    last
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

/// Derive `"owner/repo"` slug from a GitHub PR URL, falling back to `"{owner}/{repo}"`.
pub fn repo_slug_from_pr_url(pr_url: Option<&str>) -> String {
    pr_url
        .and_then(parse_github_pr_url)
        .map(|(owner, repo, _)| format!("{owner}/{repo}"))
        .unwrap_or_else(|| "{owner}/{repo}".to_string())
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

/// Check if reviewer output indicates quota exhaustion (e.g., Gemini 24h rate limit).
///
/// Requires ≥ 2 co-occurring quota-signal terms, or a single high-specificity phrase,
/// to prevent false positives from reviews that incidentally mention "quota" in code.
pub fn is_quota_exhausted(output: &str) -> bool {
    let lower = output.to_lowercase();
    // High-specificity phrase: shortcut without requiring two co-occurring indicators.
    if lower.contains("start processing again") {
        return true;
    }
    let indicators = [
        "quota",
        "rate limit",
        "24 hour",
        "processing again",
        "will be processed",
        "not available",
    ];
    indicators.iter().filter(|&&s| lower.contains(s)).count() >= 2
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

/// Task complexity assessed during the triage phase.
///
/// Used to derive dynamic `max_rounds` and `skip_agent_review` parameters so that
/// simple tasks don't waste review budget and complex tasks get extra rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageComplexity {
    /// Typo, doc fix, or single-file config change — max_rounds=2, skip agent review.
    Low,
    /// Single-module change — max_rounds=4 (current default), full review.
    Medium,
    /// Cross-file refactor or new API surface — max_rounds=8, full review.
    High,
}

/// Parse `COMPLEXITY=<low|medium|high>` from triage agent output.
///
/// The protocol requires `COMPLEXITY=` to appear on the second-to-last line
/// (just before `TRIAGE=` on the last line). Only the second-to-last non-empty
/// line is examined to prevent untrusted issue content (e.g. a line containing
/// `COMPLEXITY=low` in the issue body) from influencing classification.
///
/// Returns `TriageComplexity::Medium` if the tag is absent or unrecognised
/// (backward-compatible fallback).
pub fn parse_complexity(output: &str) -> TriageComplexity {
    let non_empty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    // second-to-last line (index len-2); need at least 2 non-empty lines
    if non_empty.len() < 2 {
        return TriageComplexity::Medium;
    }
    let candidate = non_empty[non_empty.len() - 2].trim();
    if let Some(value) = candidate.strip_prefix("COMPLEXITY=") {
        return match value.trim().to_ascii_lowercase().as_str() {
            "low" => TriageComplexity::Low,
            "high" => TriageComplexity::High,
            _ => TriageComplexity::Medium,
        };
    }
    TriageComplexity::Medium
}

/// Triage decision from the Tech Lead evaluation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageDecision {
    /// Trivial change — skip planning, go straight to implement.
    Proceed,
    /// Complex change — needs an implementation plan first.
    ProceedWithPlan,
    /// Issue is ambiguous — questions must be answered before proceeding.
    NeedsClarification,
    /// Not worth doing — skip this issue.
    Skip,
}

/// Parse the triage decision from agent output.
///
/// Looks for `TRIAGE=<decision>` on the last non-empty line.
/// Returns `None` if the output doesn't contain a valid triage line.
pub fn parse_triage(output: &str) -> Option<TriageDecision> {
    let last = last_non_empty_line(output)?;
    let value = last.strip_prefix("TRIAGE=")?;
    match value.trim() {
        "PROCEED" => Some(TriageDecision::Proceed),
        "PROCEED_WITH_PLAN" => Some(TriageDecision::ProceedWithPlan),
        "NEEDS_CLARIFICATION" => Some(TriageDecision::NeedsClarification),
        "SKIP" => Some(TriageDecision::Skip),
        _ => None,
    }
}

/// Check if the agent flagged an issue with the implementation plan.
///
/// During the implement phase, if the plan is wrong the agent outputs
/// `PLAN_ISSUE=<description>` instead of implementing. Returns the
/// description if present.
pub fn parse_plan_issue(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let line = line.trim();
        if let Some(desc) = line.strip_prefix("PLAN_ISSUE=") {
            return Some(desc.trim().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_issue_count() {
        assert_eq!(parse_issue_count("ISSUES=3\nFIXED"), Some(3));
        assert_eq!(parse_issue_count("ISSUES=0\nLGTM"), Some(0));
        assert_eq!(parse_issue_count("some output\nISSUES=12\nFIXED"), Some(12));
        assert_eq!(parse_issue_count("no issues line\nFIXED"), None);
        assert_eq!(parse_issue_count("ISSUES=abc\nFIXED"), None);
    }

    #[test]
    fn test_is_waiting() {
        assert!(is_waiting("checking reviews...\nWAITING"));
        assert!(is_waiting("WAITING\n"));
        assert!(!is_waiting("LGTM"));
        assert!(!is_waiting("FIXED"));
    }

    #[test]
    fn test_is_quota_exhausted_gemini_exact_phrase() {
        // Gemini's actual quota-warning phrasing triggers the high-specificity shortcut.
        assert!(is_quota_exhausted(
            "I will start processing again in 24 hours once the quota resets."
        ));
    }

    #[test]
    fn test_is_quota_exhausted_two_co_occurring_indicators() {
        assert!(is_quota_exhausted(
            "rate limit exceeded, quota exhausted for today"
        ));
    }

    #[test]
    fn test_is_quota_exhausted_false_for_lgtm() {
        assert!(!is_quota_exhausted("LGTM"));
    }

    #[test]
    fn test_is_quota_exhausted_false_for_normal_review() {
        let review = "\
The implementation looks clean. I noticed a potential edge case in the error \
handler: when the input is empty the function returns Ok(()) without logging, \
which could mask silent failures.\nISSUES=1\nFIXED";
        assert!(!is_quota_exhausted(review));
    }

    #[test]
    fn test_is_quota_exhausted_false_for_empty_string() {
        assert!(!is_quota_exhausted(""));
    }

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

    // --- ANSI stripping tests (issue #982) ---

    #[test]
    fn test_parse_pr_url_ansi_reset_prefix() {
        // T1: ANSI reset code on the same line as PR_URL=
        let output = "Some output\n\x1b[0mPR_URL=https://github.com/owner/repo/pull/42";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/42".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_ansi_colors_wrapping_line() {
        // T2: bold green colour codes wrapping the entire PR_URL= line
        let output = "Some output\n\x1b[1;32mPR_URL=https://github.com/owner/repo/pull/99\x1b[0m";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/99".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_truncated_no_panic() {
        // T3: truncated URL (no PR number) → None, no panic
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/foo/bar/pull/"),
            None
        );
    }

    #[test]
    fn test_parse_pr_url_multiple_returns_last() {
        // T4: agent mentions an old PR then creates a new one — return the last valid URL
        let output = "see old PR at PR_URL=https://github.com/owner/repo/pull/10\n\
                      done\n\
                      PR_URL=https://github.com/owner/repo/pull/42";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/42".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_bare_url_fallback() {
        // T5: no PR_URL= prefix but a plain GitHub PR URL is present
        let output = "Created PR: https://github.com/owner/repo/pull/77\nDone.";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/77".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_bare_url_fallback_returns_last() {
        // T5 variant: multiple bare URLs → last valid PR URL is returned
        let output = "Mentioned https://github.com/owner/repo/pull/1 earlier.\n\
                      Final result https://github.com/owner/repo/pull/55.";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/55".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_bare_issue_url_ignored() {
        // issue URLs must not match the bare URL fallback (not a /pull/ URL)
        let output = "Created https://github.com/owner/repo/issues/10";
        assert_eq!(parse_pr_url(output), None);
    }

    #[test]
    fn test_parse_pr_url_stderr_combined_output() {
        // T8: when stderr is appended to output with a separator, sentinel is found
        let stdout = "Implementing changes...\n";
        let stderr = "PR_URL=https://github.com/owner/repo/pull/88";
        let combined = format!("{stdout}\n--- stderr ---\n{stderr}");
        assert_eq!(
            parse_pr_url(&combined),
            Some("https://github.com/owner/repo/pull/88".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_rejects_javascript_scheme() {
        // SEC-03: javascript: URIs must be rejected to prevent XSS
        assert_eq!(
            parse_pr_url("PR_URL=javascript:alert(document.cookie)"),
            None
        );
        assert_eq!(parse_pr_url("output\nPR_URL=javascript:void(0)"), None);
    }

    #[test]
    fn test_parse_pr_url_rejects_shell_metacharacters() {
        // SEC-03: shell metacharacters in path segments must be rejected to
        // prevent command injection when the URL is embedded in `gh pr diff`
        // Extra path segment with command substitution
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/1/$(whoami)"),
            None
        );
        // Non-numeric PR number containing shell special chars
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/1;rm+-rf+/"),
            None
        );
        // http:// (non-GitHub, non-https) must be rejected
        assert_eq!(
            parse_pr_url("PR_URL=http://github.com/owner/repo/pull/1"),
            None
        );
    }

    #[test]
    fn test_parse_pr_url_strips_fragment() {
        // SEC-03: the fragment must be stripped from the returned URL so that a
        // malicious fragment (e.g. `#';touch /tmp/pwn;'`) cannot escape shell
        // quoting when the URL is embedded in `gh pr diff '{pr_url}'`.
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/1#';touch /tmp/pwn;'"),
            Some("https://github.com/owner/repo/pull/1".to_string()),
        );
        // Legitimate discussion fragments are also stripped (not needed by gh cli).
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/42#discussion_r123456"),
            Some("https://github.com/owner/repo/pull/42".to_string()),
        );
    }

    #[test]
    fn test_parse_pr_url_accepts_path_suffixes() {
        // Regression: URLs with /files or /commits suffix produced by GitHub must
        // be accepted — the previous strict 4-segment check silently dropped them,
        // causing the review loop to be skipped entirely.
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/42/files"),
            Some("https://github.com/owner/repo/pull/42/files".to_string())
        );
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/42/commits"),
            Some("https://github.com/owner/repo/pull/42/commits".to_string())
        );
        // Trailing slash must also be accepted.
        assert_eq!(
            parse_pr_url("PR_URL=https://github.com/owner/repo/pull/42/"),
            Some("https://github.com/owner/repo/pull/42".to_string())
        );
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

    #[test]
    fn test_parse_created_issue_number_sentinel() {
        let output = "some output\nCREATED_ISSUE=99\nmore output";
        assert_eq!(parse_created_issue_number(output), Some(99));
    }

    #[test]
    fn test_parse_created_issue_number_returns_last_sentinel() {
        // When the agent emits two sentinel lines (self-correction), take the last.
        let output = "CREATED_ISSUE=10\nCREATED_ISSUE=20";
        assert_eq!(parse_created_issue_number(output), Some(20));
    }

    #[test]
    fn test_parse_created_issue_number_github_url_ignored() {
        // URL scanning was removed to prevent false positives — bare URLs must not match.
        let output = "Created issue https://github.com/owner/repo/issues/42";
        assert_eq!(parse_created_issue_number(output), None);
    }

    #[test]
    fn test_parse_created_issue_number_pr_url_ignored() {
        // PR URLs must not match either.
        let output = "PR_URL=https://github.com/owner/repo/pull/42";
        assert_eq!(parse_created_issue_number(output), None);
    }

    #[test]
    fn test_parse_created_issue_number_bare_hash_ignored() {
        // Bare #N references must not match.
        let output = "fixes #55 as requested";
        assert_eq!(parse_created_issue_number(output), None);
    }

    #[test]
    fn test_parse_created_issue_number_no_match() {
        assert_eq!(parse_created_issue_number("no url here"), None);
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
    fn test_parse_pr_url_with_trailing_whitespace() {
        let output = "done\nPR_URL=https://github.com/o/r/pull/5  \n";
        let url = parse_pr_url(output).unwrap();
        assert_eq!(extract_pr_number(&url), Some(5));
    }

    #[test]
    fn test_is_lgtm_exact() {
        assert!(is_lgtm("some output\nLGTM"));
        assert!(is_lgtm("LGTM"));
        assert!(is_lgtm("some output\nLGTM\n"));
    }

    #[test]
    fn test_is_lgtm_false_positive() {
        // "LGTM" embedded in another word or non-final line should NOT match
        assert!(!is_lgtm("LGTM but actually not done"));
        assert!(!is_lgtm(
            "I think it looks LGTM but needs one more fix\nFIXED"
        ));
        assert!(!is_lgtm("somethingLGTM"));
        assert!(!is_lgtm("notLGTM\n"));
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

    #[test]
    fn test_parse_triage_all_variants() {
        assert_eq!(
            parse_triage("Assessment done.\nTRIAGE=PROCEED"),
            Some(TriageDecision::Proceed)
        );
        assert_eq!(
            parse_triage("Complex issue.\nTRIAGE=PROCEED_WITH_PLAN"),
            Some(TriageDecision::ProceedWithPlan)
        );
        assert_eq!(
            parse_triage("Unclear.\nTRIAGE=NEEDS_CLARIFICATION"),
            Some(TriageDecision::NeedsClarification)
        );
        assert_eq!(
            parse_triage("Not worth it.\nTRIAGE=SKIP"),
            Some(TriageDecision::Skip)
        );
    }

    #[test]
    fn test_parse_triage_invalid() {
        assert_eq!(parse_triage("no triage here"), None);
        assert_eq!(parse_triage("TRIAGE=UNKNOWN"), None);
        assert_eq!(parse_triage(""), None);
    }

    #[test]
    fn test_parse_triage_trailing_whitespace() {
        assert_eq!(
            parse_triage("done\nTRIAGE=PROCEED\n"),
            Some(TriageDecision::Proceed)
        );
    }

    #[test]
    fn test_parse_plan_issue() {
        assert_eq!(
            parse_plan_issue("Working on it...\nPLAN_ISSUE=The plan missed error handling"),
            Some("The plan missed error handling".to_string())
        );
        assert_eq!(parse_plan_issue("PR_URL=https://example.com/pull/1"), None);
    }

    #[test]
    fn test_parse_complexity_all_variants() {
        // parse_complexity checks the second-to-last non-empty line, so inputs need >= 2 lines.
        // Real agent output ends with TRIAGE=<decision> on the last line.
        assert_eq!(
            parse_complexity("COMPLEXITY=low\nTRIAGE=PROCEED"),
            TriageComplexity::Low
        );
        assert_eq!(
            parse_complexity("COMPLEXITY=medium\nTRIAGE=PROCEED"),
            TriageComplexity::Medium
        );
        assert_eq!(
            parse_complexity("COMPLEXITY=high\nTRIAGE=PROCEED"),
            TriageComplexity::High
        );
        // Case-insensitive
        assert_eq!(
            parse_complexity("COMPLEXITY=LOW\nTRIAGE=PROCEED"),
            TriageComplexity::Low
        );
        assert_eq!(
            parse_complexity("COMPLEXITY=HIGH\nTRIAGE=PROCEED"),
            TriageComplexity::High
        );
    }

    #[test]
    fn test_parse_complexity_fallback() {
        // Missing tag → Medium
        assert_eq!(parse_complexity("TRIAGE=PROCEED"), TriageComplexity::Medium);
        assert_eq!(parse_complexity(""), TriageComplexity::Medium);
        // Unknown value → Medium
        assert_eq!(
            parse_complexity("COMPLEXITY=extreme"),
            TriageComplexity::Medium
        );
        assert_eq!(parse_complexity("COMPLEXITY="), TriageComplexity::Medium);
    }

    #[test]
    fn test_parse_complexity_with_triage_combo() {
        let output = "This is a simple typo fix.\nCOMPLEXITY=high\nTRIAGE=PROCEED";
        assert_eq!(parse_complexity(output), TriageComplexity::High);
        assert_eq!(parse_triage(output), Some(TriageDecision::Proceed));
    }

    #[test]
    fn test_parse_triage_unaffected_by_complexity_line() {
        // COMPLEXITY= before TRIAGE= must not break parse_triage
        let output = "Assessment here.\nCOMPLEXITY=low\nTRIAGE=PROCEED";
        assert_eq!(parse_triage(output), Some(TriageDecision::Proceed));

        let output2 = "Assessment here.\nCOMPLEXITY=high\nTRIAGE=PROCEED_WITH_PLAN";
        assert_eq!(parse_triage(output2), Some(TriageDecision::ProceedWithPlan));
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
    fn test_malformed_reviewer_output_has_no_issues() {
        let malformed = "I looked at the diff and it seems fine to me.";
        assert!(extract_review_issues(malformed).is_empty());
    }
}
