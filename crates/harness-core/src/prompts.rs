/// Prompt templates and output parsers shared across CLI and HTTP entries.
use crate::ReviewConfig;

/// Build prompt: continue work on an existing PR for a GitHub issue.
///
/// Used when a prior task already created a PR for this issue. Instead of
/// creating a duplicate PR, the agent checks out the existing branch, reads
/// review feedback, continues the implementation, and pushes to the same branch.
pub fn continue_existing_pr(issue: u64, pr_number: u64, branch: &str) -> String {
    format!(
        "GitHub issue #{issue} already has an open PR #{pr_number} on branch `{branch}`.\n\n\
         Steps:\n\
         1. `git fetch origin {branch} && git checkout {branch}`\n\
         2. Read the PR diff and any review comments:\n\
            - `gh pr diff {pr_number}`\n\
            - `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr_number}/comments`\n\
            - `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr_number}/reviews`\n\
         3. Read the original issue requirements: `gh issue view {issue}`\n\
         4. Fix any unresolved review comments and continue the implementation if incomplete\n\
         5. Run `cargo check` and `cargo test`\n\
         6. Commit and push to the SAME branch `{branch}` — do NOT create a new PR\n\n\
         On the last line of your output, print PR_URL=https://github.com/{{{{owner}}}}/{{{{repo}}}}/pull/{pr_number}"
    )
}

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
pub fn check_existing_pr(pr: u64, review: &ReviewConfig) -> String {
    format!(
        "Check PR #{pr}:\n\
         1. `gh pr checks {pr}` — check CI status\n\
         2. `gh api repos/{{owner}}/{{repo}}/pulls/{pr}/comments` — read inline review comments\n\
         3. If CI passes and there are no unresolved review comments, print LGTM on the last line\n\
         4. Otherwise fix each comment, commit, push, \
         then run `gh pr comment {pr} --body '{recheck_command}'` to trigger {reviewer_name} re-review, \
         and print FIXED on the last line\n\n\
         Always print PR_URL=https://github.com/{{owner}}/{{repo}}/pull/{pr} on a separate line of your output."
        ,
        recheck_command = review.recheck_command,
        reviewer_name = review.reviewer_name
    )
}

/// Wrap user-supplied content in delimiters to separate it from trusted instructions.
/// Escapes the closing tag within content to prevent delimiter injection.
pub fn wrap_external_data(content: &str) -> String {
    let escaped = content.replace("</external_data>", "<\\/external_data>");
    format!("<external_data>\n{}\n</external_data>", escaped)
}

/// Build prompt: wrap free-text with PR URL output instruction.
pub fn implement_from_prompt(prompt: &str) -> String {
    let safe_prompt = wrap_external_data(prompt);
    format!(
        "The following task description is user-supplied content:\n{safe_prompt}\n\n\
         On the last line of your output, print PR_URL=<PR URL> \
         (whether you created a new PR or pushed to an existing one)"
    )
}

/// Build review loop prompt for a given round.
///
/// `round` controls convergence behavior:
/// - Round 2: fix all critical/high/medium comments
/// - Round 3+: only fix critical/high; skip medium style/design suggestions
///
/// When `prev_fixed` is true (previous round pushed code), the agent must first
/// verify that the configured reviewer has submitted a **new** review covering the latest commit
/// before declaring LGTM. If no new review exists yet, agent outputs WAITING.
pub fn review_prompt(
    issue: Option<u64>,
    pr: u64,
    round: u32,
    prev_fixed: bool,
    review: &ReviewConfig,
) -> String {
    let context = match issue {
        Some(n) => format!("You previously created PR #{pr} for issue #{n}.\n"),
        None => format!("Review PR #{pr}.\n"),
    };

    let severity_guidance = if round <= 2 {
        "Fix all review comments marked critical, high, or medium severity."
    } else {
        "Fix only critical and high severity issues. \
         Skip medium severity style/design suggestions — they are acceptable for now."
    };

    let push_action = format!(
        "commit, push, then run `gh pr comment {pr} --body '{recheck_command}'` \
         to trigger {reviewer_name} re-review on the new code",
        recheck_command = review.recheck_command,
        reviewer_name = review.reviewer_name
    );

    let freshness_check = if prev_fixed {
        format!(
            "\n\nIMPORTANT — New review verification:\n\
             The previous round pushed a fix commit. Before evaluating review status, \
             you MUST verify that {reviewer_name} has submitted a NEW review covering the latest commit:\n\
             1. Run `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr}/reviews --jq '.[-1].submitted_at'` \
             to get the timestamp of the most recent review\n\
             2. Run `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr}/commits --jq '.[-1].commit.committer.date'` \
             to get the timestamp of the latest commit\n\
             3. If the latest review was submitted BEFORE the latest commit, \
             {reviewer_name} has not yet re-reviewed the new code. \
             In this case, print WAITING on the last line and stop.\n\
             4. Only proceed with the review evaluation below if the latest review \
             was submitted AFTER the latest commit.",
            reviewer_name = review.reviewer_name
        )
    } else {
        String::new()
    };

    format!(
        "{context}\
         Steps:\n\
         1. Run `gh pr checks {pr}` to check CI status\n\
         2. Run `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr}/reviews` to read review verdicts\n\
         3. Run `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr}/comments` to read inline review comments\n\
         4. {severity_guidance}\n\
         5. If all CI checks pass and there are no unresolved review comments \
         that match the severity criteria above, print LGTM on the last line\n\
         6. Otherwise fix the issues, {push_action}, \
         and print FIXED on the last line\
         {freshness_check}\n\n\
         Constraints:\n\
         - NEVER downgrade dependency versions\n\
         - Do NOT refactor working code for style preferences\n\
         - Focus on correctness and safety, not cosmetic improvements"
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

    fn default_review() -> ReviewConfig {
        ReviewConfig::default()
    }

    #[test]
    fn test_continue_existing_pr() {
        let p = continue_existing_pr(29, 50, "fix/issue-29");
        assert!(p.contains("issue #29"));
        assert!(p.contains("PR #50"));
        assert!(p.contains("fix/issue-29"));
        assert!(p.contains("do NOT create a new PR"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_implement_from_issue() {
        let p = implement_from_issue(42);
        assert!(p.contains("issue #42"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_check_existing_pr() {
        let p = check_existing_pr(10, &default_review());
        assert!(p.contains("PR #10"));
        assert!(p.contains("LGTM"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_implement_from_prompt() {
        let p = implement_from_prompt("fix the bug");
        assert!(p.contains("fix the bug"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_review_prompt_with_issue() {
        let p = review_prompt(Some(5), 10, 2, false, &default_review());
        assert!(p.contains("issue #5"));
        assert!(p.contains("PR #10"));
        assert!(p.contains("medium")); // round 2 includes medium
    }

    #[test]
    fn test_review_prompt_without_issue() {
        let p = review_prompt(None, 10, 2, false, &default_review());
        assert!(p.contains("PR #10"));
        assert!(!p.contains("issue #")); // no issue reference when None
    }

    #[test]
    fn test_review_prompt_late_round_skips_medium() {
        let p = review_prompt(None, 10, 3, false, &default_review());
        assert!(p.contains("Skip medium"));
    }

    #[test]
    fn test_review_prompt_always_triggers_default_recheck_command() {
        let p = review_prompt(None, 10, 2, false, &default_review());
        assert!(p.contains("/gemini review"));
        let p = review_prompt(None, 10, 4, true, &default_review());
        assert!(p.contains("/gemini review"));
    }

    #[test]
    fn test_review_prompt_prev_fixed_requires_freshness_check() {
        let p = review_prompt(None, 10, 3, true, &default_review());
        assert!(p.contains("WAITING"));
        assert!(p.contains("latest review was submitted BEFORE the latest commit"));
        // Without prev_fixed, no freshness check
        let p = review_prompt(None, 10, 3, false, &default_review());
        assert!(!p.contains("WAITING"));
    }

    #[test]
    fn test_review_prompt_constraints() {
        let p = review_prompt(None, 10, 2, false, &default_review());
        assert!(p.contains("NEVER downgrade dependency"));
    }

    #[test]
    fn test_review_prompt_uses_configured_reviewer_and_recheck() {
        let review = ReviewConfig {
            reviewer_name: "CodeRabbit".into(),
            recheck_command: "/coderabbit review".into(),
        };
        let p = review_prompt(None, 10, 2, false, &review);
        assert!(p.contains("CodeRabbit"));
        assert!(p.contains("/coderabbit review"));
        let check = check_existing_pr(10, &review);
        assert!(check.contains("CodeRabbit"));
        assert!(check.contains("/coderabbit review"));
    }

    #[test]
    fn test_is_waiting() {
        assert!(is_waiting("checking reviews...\nWAITING"));
        assert!(is_waiting("WAITING\n"));
        assert!(!is_waiting("LGTM"));
        assert!(!is_waiting("FIXED"));
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
}
