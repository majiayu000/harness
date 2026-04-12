//! Task lifecycle prompt builders: triage, plan, implement, check PR.

use super::{wrap_external_data, PromptParts};
use crate::config::project::GitConfig;

/// Build prompt: continue work on an existing PR for a GitHub issue.
///
/// Used when a prior task already created a PR for this issue. Instead of
/// creating a duplicate PR, the agent checks out the existing branch, reads
/// review feedback, continues the implementation, and pushes to the same branch.
pub fn continue_existing_pr(issue: u64, pr_number: u64, branch: &str, repo: &str) -> String {
    format!(
        "GitHub issue #{issue} already has an open PR #{pr_number} on branch `{branch}`.\n\n\
         Steps:\n\
         1. `git fetch origin {branch} && git checkout {branch}`\n\
         2. Read the PR diff and any review comments:\n\
            - `gh pr diff {pr_number}`\n\
            - `gh api repos/{repo}/pulls/{pr_number}/comments`\n\
            - `gh api repos/{repo}/pulls/{pr_number}/reviews`\n\
         3. Read the original issue requirements: `gh issue view {issue}`\n\
         4. Fix any unresolved review comments and continue the implementation if incomplete\n\
         5. Run `cargo check` and `cargo test`\n\
         6. Commit and push to the SAME branch `{branch}` — do NOT create a new PR\n\n\
         On the last line of your output, print PR_URL=https://github.com/{repo}/pull/{pr_number}"
    )
}

/// Build triage prompt: Tech Lead evaluates whether an issue is worth implementing.
///
/// Outputs a triage decision on the last line:
/// - `TRIAGE=PROCEED` — trivial change, skip planning, go straight to implement
/// - `TRIAGE=PROCEED_WITH_PLAN` — complex change, needs a plan phase first
/// - `TRIAGE=NEEDS_CLARIFICATION` — issue is ambiguous, list questions
/// - `TRIAGE=SKIP` — not worth doing, out of scope, or duplicate
pub fn triage_prompt(issue: u64) -> PromptParts {
    PromptParts {
        static_instructions: format!(
            "You are a Tech Lead evaluating GitHub issue #{issue} before any code is written.\n\n\
             Your job is to THINK before committing engineering effort. Read the issue carefully, then assess:\n\n\
             1. **Complexity**: Is this trivial (typo, config, 1-file change), moderate (2-3 files, \
             clear scope), or complex (4+ files, new API surface, architectural change)?\n\
             2. **Clarity**: Are the requirements specific enough to implement without guessing? \
             What assumptions would an engineer have to make?\n\
             3. **Risk**: What could go wrong in production? Are there edge cases, \
             backward compatibility concerns, or security implications?\n\
             4. **Scope**: Does this issue try to do too much? Should it be split?\n\n\
             Based on your assessment, choose ONE recommendation:\n\
             - PROCEED — trivial/clear change, no planning needed, go straight to implementation\n\
             - PROCEED_WITH_PLAN — non-trivial change that needs an implementation plan first\n\
             - NEEDS_CLARIFICATION — issue is ambiguous; list the specific questions that must be answered\n\
             - SKIP — not worth doing (explain why: duplicate, out of scope, or fundamentally flawed)\n\n\
             Output format: Write your assessment (2-5 sentences), then on the second-to-last line print:\n\
             COMPLEXITY=<low|medium|high> (low=typo/doc fix, medium=single-module change, high=cross-file refactor)\n\
             Then on the LAST line print:\n\
             TRIAGE=<recommendation>"
        ),
        context: String::new(),
        dynamic_payload: String::new(),
    }
}

/// Build plan prompt: Architect designs an implementation plan before coding begins.
///
/// Takes the triage output as context. Outputs `PLAN=READY` on the last line.
pub fn plan_prompt(issue: u64, triage_assessment: &str) -> PromptParts {
    let safe_triage = wrap_external_data(triage_assessment);
    PromptParts {
        static_instructions: format!(
            "You are a Software Architect designing the implementation plan for GitHub issue #{issue}.\n\n\
             A Tech Lead already assessed this issue and recommended it needs a plan:\n\
             {safe_triage}\n\n\
             Create a concise implementation plan:\n\n\
             1. **Files to modify** — list each file with a one-line rationale\n\
             2. **Files to create** — only if truly needed (prefer modifying existing files)\n\
             3. **Key changes** — the 2-3 most important code changes (function signatures, \
             types, data flow)\n\
             4. **Test plan** — what to test, key edge cases, expected test count\n\
             5. **Risks** — what could go wrong, how to mitigate\n\n\
             Keep the plan actionable. An engineer should be able to follow it without asking questions.\n\
             Do NOT write any code — only describe what to do.\n\n\
             On the LAST line, print: PLAN=READY"
        ),
        context: String::new(),
        dynamic_payload: String::new(),
    }
}

/// Build prompt parts: implement from a GitHub issue, create PR.
///
/// When `plan` is provided, the engineer follows the plan instead of figuring
/// out the approach from scratch. When `plan` is `None`, the engineer reads
/// the issue directly and decides the approach (used for trivial issues that
/// skip the plan phase).
///
/// Returns a [`PromptParts`] with three layers for future prompt caching.
pub fn implement_from_issue(
    issue: u64,
    git: Option<&GitConfig>,
    plan: Option<&str>,
) -> PromptParts {
    let git_line = git_config_line(git);
    let plan_section = match plan {
        Some(p) => {
            let safe_plan = wrap_external_data(p);
            format!(
                "\n\nAn architect created the following implementation plan. Follow it closely.\n\
                 If the plan is wrong or incomplete, output PLAN_ISSUE=<description> instead of \
                 deviating silently.\n{safe_plan}\n"
            )
        }
        None => String::new(),
    };
    PromptParts {
        static_instructions: format!(
            "You are a Senior Engineer implementing a change for GitHub issue #{issue}.\n\n\
             Your job is to write correct, tested, minimal code. Do not add features beyond \
             what the issue requests. Do not refactor unrelated code.\n\
             {plan_section}\n\
             Steps:\n\
             1. Read the issue: `gh issue view {issue}`\n\
             2. Understand the requirements and existing code\n\
             3. Implement the change with the minimum necessary modifications\n\
             4. Run `cargo check` and `cargo test` — fix any failures before proceeding\n\
             5. Create a feature branch, commit with a descriptive message, push\n\
             6. Create a PR with `gh pr create`"
        ),
        context: git_line,
        dynamic_payload: "On the last line of your output, print PR_URL=<full PR URL>".to_string(),
    }
}

/// Build prompt: check an existing PR's CI and review status.
///
/// When `prev_fixed` is true (the previous round pushed a fix commit), the agent
/// must first verify that `reviewer_name` has submitted a **new** review covering
/// the latest commit before declaring LGTM. If no new review exists yet, the agent
/// outputs WAITING.
pub fn check_existing_pr(
    pr: u64,
    review_bot_command: &str,
    repo: &str,
    reviewer_name: &str,
    prev_fixed: bool,
) -> String {
    let body = shell_single_quote(review_bot_command);
    let freshness_check = if prev_fixed {
        let login_filter =
            format!("[.[] | select(.user.login == \"{reviewer_name}\")] | last | .submitted_at");
        format!(
            "\n\nIMPORTANT — New review verification:\n\
             The previous round pushed a fix commit. Before evaluating review status, \
             you MUST verify that {reviewer_name} has submitted a NEW review covering the latest commit:\n\
             1. Run `gh api repos/{repo}/pulls/{pr}/reviews \
             --jq '{login_filter}'` \
             to get the timestamp of {reviewer_name}'s most recent review\n\
             2. Run `gh api repos/{repo}/pulls/{pr}/commits --jq '.[-1].commit.committer.date'` \
             to get the timestamp of the latest commit\n\
             3. If {reviewer_name}'s latest review was submitted BEFORE the latest commit \
             (or no review from {reviewer_name} exists), \
             {reviewer_name} has not yet re-reviewed the new code. \
             In this case, print WAITING on the last line and stop.\n\
             4. Only proceed with the checks below if {reviewer_name}'s latest review \
             was submitted AFTER the latest commit."
        )
    } else {
        String::new()
    };
    format!(
        "Check PR #{pr}:{freshness_check}\n\
         1. Run `gh pr view {pr} --json statusCheckRollup` — parse the JSON. \
         CI passes only if the `state` field in the `statusCheckRollup` object is `SUCCESS`\n\
         2. `gh api repos/{repo}/pulls/{pr}/comments` — read inline review comments\n\
         3. If CI passes and there are no unresolved review comments, print LGTM on the last line\n\
         4. Otherwise fix each comment, commit, push, \
         then run `gh pr comment {pr} --body {body}` to trigger re-review, \
         and print FIXED on the last line\n\n\
         Always print PR_URL=https://github.com/{repo}/pull/{pr} on a separate line of your output."
    )
}

/// Build prompt: wrap free-text with PR URL output instruction.
///
/// If `git` is provided, git instructions are appended before the PR_URL line.
pub fn implement_from_prompt(prompt: &str, git: Option<&GitConfig>) -> String {
    let safe_prompt = wrap_external_data(prompt);
    let git_line = git_config_line(git);
    format!(
        "The following task description is user-supplied content:\n{safe_prompt}\n\n\
         {git_line}\
         On the last line of your output, print PR_URL=<PR URL> \
         (whether you created a new PR or pushed to an existing one)"
    )
}

/// Build a git instruction line from optional GitConfig.
/// Returns an empty string when `git` is None, or a sentence ending with "\n" when Some.
fn git_config_line(git: Option<&GitConfig>) -> String {
    match git {
        None => String::new(),
        Some(g) => format!(
            " Create your PR targeting the {} branch on the {} remote. \
             Use branch prefix {}.\n",
            g.base_branch, g.remote, g.branch_prefix
        ),
    }
}

/// Wrap `s` in POSIX single quotes, escaping any embedded single quotes via `'\''`.
///
/// This ensures the value is treated as literal data by the shell and cannot
/// break out of the quoting context or inject shell metacharacters.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_continue_existing_pr() {
        let p = continue_existing_pr(29, 50, "fix/issue-29", "owner/repo");
        assert!(p.contains("issue #29"));
        assert!(p.contains("PR #50"));
        assert!(p.contains("fix/issue-29"));
        assert!(p.contains("do NOT create a new PR"));
        assert!(p.contains("PR_URL=https://github.com/owner/repo/pull/50"));
        assert!(p.contains("repos/owner/repo/pulls/50/comments"));
    }

    #[test]
    fn test_implement_from_issue() {
        let p = implement_from_issue(42, None, None).to_prompt_string();
        assert!(p.contains("issue #42"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_prompt_parts_to_prompt_string_no_git() {
        let parts = implement_from_issue(42, None, None);
        let s = parts.to_prompt_string();
        assert!(s.contains("issue #42"));
        assert!(s.contains("PR_URL=<full PR URL>"));
        assert!(s.contains("Senior Engineer"));
        assert!(s.contains("cargo check"));
        assert!(s.contains("gh pr create"));
        assert!(parts.context.is_empty());
    }

    #[test]
    fn test_prompt_parts_to_prompt_string_with_git() {
        let git = GitConfig {
            base_branch: "main".to_string(),
            remote: "origin".to_string(),
            branch_prefix: "feat/".to_string(),
        };
        let parts = implement_from_issue(7, Some(&git), None);
        assert!(parts.context.contains("main"));
        assert!(parts.context.contains("origin"));
        assert!(parts.context.contains("feat/"));
        let s = parts.to_prompt_string();
        assert!(s.contains("issue #7"));
        assert!(s.contains("main"));
        assert!(s.contains("PR_URL=<full PR URL>"));
    }

    #[test]
    fn test_implement_from_issue_with_plan() {
        let plan = "1. Modify src/lib.rs\n2. Add test";
        let p = implement_from_issue(42, None, Some(plan)).to_prompt_string();
        assert!(p.contains("issue #42"));
        assert!(p.contains("implementation plan"));
        assert!(p.contains("Modify src/lib.rs"));
        assert!(p.contains("PLAN_ISSUE="));
    }

    #[test]
    fn test_prompt_parts_fields_are_accessible() {
        let parts = implement_from_issue(99, None, None);
        assert!(parts.static_instructions.contains("issue #99"));
        assert!(parts.dynamic_payload.contains("PR_URL="));
        assert!(parts.context.is_empty());
    }

    #[test]
    fn test_check_existing_pr() {
        let p = check_existing_pr(
            10,
            "/gemini review",
            "owner/repo",
            "gemini-code-assist[bot]",
            false,
        );
        assert!(p.contains("PR #10"));
        assert!(p.contains("LGTM"));
        assert!(p.contains("PR_URL=https://github.com/owner/repo/pull/10"));
        assert!(p.contains("repos/owner/repo/pulls/10/comments"));
        assert!(
            p.contains("statusCheckRollup"),
            "must use statusCheckRollup for CI status"
        );
        assert!(
            p.contains("state") && p.contains("SUCCESS"),
            "must instruct agent to check state field for SUCCESS"
        );
    }

    #[test]
    fn test_implement_from_prompt() {
        let p = implement_from_prompt("fix the bug", None);
        assert!(p.contains("fix the bug"));
        assert!(p.contains("PR_URL="));
    }

    #[test]
    fn test_check_existing_pr_shell_quoting() {
        // A command containing a single quote must not break single-quoting
        let p = check_existing_pr(5, "it's a test", "owner/repo", "bot[bot]", false);
        assert!(
            p.contains(r"'it'\''s a test'"),
            "single quote must be escaped"
        );
    }

    #[test]
    fn test_triage_prompt_contains_issue_and_recommendations() {
        let parts = triage_prompt(42);
        let s = parts.to_prompt_string();
        assert!(s.contains("issue #42"));
        assert!(s.contains("Tech Lead"));
        assert!(s.contains("PROCEED"));
        assert!(s.contains("PROCEED_WITH_PLAN"));
        assert!(s.contains("NEEDS_CLARIFICATION"));
        assert!(s.contains("SKIP"));
        assert!(s.contains("TRIAGE="));
    }

    #[test]
    fn test_plan_prompt_contains_triage_context() {
        let parts = plan_prompt(7, "Moderate complexity, 3 files affected");
        let s = parts.to_prompt_string();
        assert!(s.contains("issue #7"));
        assert!(s.contains("Architect"));
        assert!(s.contains("Moderate complexity"));
        assert!(s.contains("PLAN=READY"));
    }

    #[test]
    fn test_triage_prompt_contains_complexity() {
        let p = triage_prompt(42).to_prompt_string();
        assert!(
            p.contains("COMPLEXITY="),
            "triage prompt must instruct COMPLEXITY= output"
        );
        assert!(
            p.contains("TRIAGE="),
            "triage prompt must instruct TRIAGE= output"
        );
    }
}
