/// Prompt templates and output parsers shared across CLI and HTTP entries.

/// Build prompt: implement from a GitHub issue, create PR.
pub fn implement_from_issue(issue: u64) -> String {
    format!(
        "读 GitHub issue #{issue}，理解需求，在当前项目中实现代码，\
         运行 cargo check 和 cargo test，创建功能分支，commit，push，\
         用 gh pr create 创建 PR。\
         完成后在输出的最后一行单独输出 PR_URL=<完整PR URL>"
    )
}

/// Build prompt: check an existing PR's CI and review status.
pub fn check_existing_pr(pr: u64) -> String {
    format!(
        "检查 PR #{pr} 的 CI 状态和 review comments。\
         如果有问题就修复代码，commit，push，\
         然后用 `gh pr comment {pr} --body '/gemini review'` 触发 review bot 重新审查。\
         如果没有问题，在最后一行单独输出 LGTM。\
         否则在最后一行单独输出 FIXED。"
    )
}

/// Build prompt: wrap free-text with PR URL output instruction.
pub fn implement_from_prompt(prompt: &str) -> String {
    format!(
        "{prompt}\n\n\
         完成后如果创建了 PR，在输出的最后一行单独输出 PR_URL=<完整PR URL>"
    )
}

/// Build review loop prompt for a given round.
pub fn review_prompt(issue: Option<u64>, pr: u64) -> String {
    let context = match issue {
        Some(n) => format!("你之前为 issue #{n} 创建了 PR #{pr}。\n"),
        None => format!("检查 PR #{pr}。\n"),
    };

    format!(
        "{context}\
         现在用 gh pr checks 检查 CI 状态，\
         用 gh pr view 和 gh api 读取 review comments。\
         如果 CI 通过且没有需要处理的 review comments，在最后一行单独输出 LGTM。\
         否则根据反馈修复代码，commit，push，\
         然后用 `gh pr comment {pr} --body '/gemini review'` 触发 review bot 重新审查，\
         在最后一行单独输出 FIXED。"
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
