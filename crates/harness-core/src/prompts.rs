/// Prompt templates and output parsers shared across CLI and HTTP entries.

/// Build prompt: implement from a GitHub issue, create PR.
pub fn implement_from_issue(issue: u64) -> String {
    format!(
        "Read GitHub issue #{issue}, understand the requirements, implement the code in this project, \
         run cargo check and cargo test, create a feature branch, commit, push, \
         and create a PR with gh pr create. \
         On the last line of your output, print PR_URL=<full PR URL>"
    )
}

/// Build prompt: check an existing PR's CI and review status.
pub fn check_existing_pr(pr: u64) -> String {
    format!(
        "Check PR #{pr}:\n\
         1. `gh pr checks {pr}` — check CI status\n\
         2. `gh api repos/{{owner}}/{{repo}}/pulls/{pr}/comments` — read inline review comments\n\
         3. If CI passes and there are no unresolved review comments, print LGTM on the last line\n\
         4. Otherwise fix each comment, commit, push, \
         then run `gh pr comment {pr} --body '/gemini review'` to trigger re-review, \
         and print FIXED on the last line"
    )
}

/// Build prompt: wrap free-text with PR URL output instruction.
pub fn implement_from_prompt(prompt: &str) -> String {
    format!(
        "{prompt}\n\n\
         On the last line of your output, print PR_URL=<PR URL> \
         (whether you created a new PR or pushed to an existing one)"
    )
}

/// Build review loop prompt for a given round.
pub fn review_prompt(issue: Option<u64>, pr: u64) -> String {
    let context = match issue {
        Some(n) => format!("You previously created PR #{pr} for issue #{n}.\n"),
        None => format!("Review PR #{pr}.\n"),
    };

    format!(
        "{context}\
         Steps:\n\
         1. Run `gh pr checks {pr}` to check CI status\n\
         2. Run `gh api repos/{{owner}}/{{repo}}/pulls/{pr}/reviews` to read review verdicts\n\
         3. Run `gh api repos/{{owner}}/{{repo}}/pulls/{pr}/comments` to read inline review comments\n\
         4. If all CI checks pass and there are no unresolved review comments \
         (including bot suggestions marked high/medium), print LGTM on the last line\n\
         5. Otherwise fix the code based on each review comment, commit, push, \
         then run `gh pr comment {pr} --body '/gemini review'` to trigger the review bot, \
         and print FIXED on the last line"
    )
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
pub fn extract_pr_number(url: &str) -> Option<u64> {
    url.split('/').last()?.parse().ok()
}

/// Check if agent output ends with LGTM (strict last-line check).
pub fn is_lgtm(output: &str) -> bool {
    output.trim().ends_with("LGTM")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_implement_from_issue() {
        let p = implement_from_issue(42);
        assert!(p.contains("issue #42"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_check_existing_pr() {
        let p = check_existing_pr(10);
        assert!(p.contains("PR #10"));
        assert!(p.contains("LGTM"));
    }

    #[test]
    fn test_implement_from_prompt() {
        let p = implement_from_prompt("fix the bug");
        assert!(p.contains("fix the bug"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_review_prompt_with_issue() {
        let p = review_prompt(Some(5), 10);
        assert!(p.contains("issue #5"));
        assert!(p.contains("PR #10"));
    }

    #[test]
    fn test_review_prompt_without_issue() {
        let p = review_prompt(None, 10);
        assert!(p.contains("PR #10"));
        assert!(!p.contains("issue"));
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

    #[test]
    fn test_extract_pr_number() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42"),
            Some(42)
        );
    }

    #[test]
    fn test_extract_pr_number_invalid() {
        assert_eq!(extract_pr_number("https://github.com/owner/repo/pull/"), None);
    }

    #[test]
    fn test_is_lgtm_exact() {
        assert!(is_lgtm("some output\nLGTM"));
        assert!(is_lgtm("LGTM"));
        assert!(is_lgtm("some output\nLGTM\n"));
    }

    #[test]
    fn test_is_lgtm_false_positive() {
        // "LGTM" in middle of output should NOT match
        assert!(!is_lgtm("LGTM but actually not done"));
        assert!(!is_lgtm("I think it looks LGTM but needs one more fix\nFIXED"));
    }
}
