//! Implementation phase prompts.

use super::helpers::{git_config_line, shell_single_quote, wrap_external_data};
use super::PromptParts;

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
    git: Option<&crate::config::project::GitConfig>,
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

/// Build prompt: wrap free-text with PR URL output instruction.
///
/// If `git` is provided, git instructions are appended before the PR_URL line.
pub fn implement_from_prompt(
    prompt: &str,
    git: Option<&crate::config::project::GitConfig>,
) -> String {
    let safe_prompt = wrap_external_data(prompt);
    let git_line = git_config_line(git);
    format!(
        "The following task description is user-supplied content:\n{safe_prompt}\n\n\
         {git_line}\
         On the last line of your output, print PR_URL=<PR URL> \
         (whether you created a new PR or pushed to an existing one)"
    )
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

/// Build prompt: adopt a GC draft — create branch, commit applied files, push, open PR.
///
/// The draft's artifact files have already been written to disk before this prompt is
/// dispatched. The agent's job is to branch, commit those files, and open a PR.
pub fn gc_adopt_prompt(
    draft_id: &str,
    rationale: &str,
    validation: &str,
    artifact_paths: &[&str],
) -> String {
    let paths = artifact_paths.join("\n");
    let safe_paths = wrap_external_data(&paths);
    let safe_rationale = wrap_external_data(rationale);
    let safe_validation = wrap_external_data(validation);
    format!(
        "GC has applied the following files to disk:\n{safe_paths}\n\n\
         Rationale:\n{safe_rationale}\n\n\
         Validation to run before committing:\n{safe_validation}\n\n\
         Steps:\n\
         1. Create branch `gc/{draft_id}` from the default branch\n\
         2. `git add` the files listed above\n\
         3. Run the validation step(s) above and fix any errors before committing\n\
         4. Commit with a descriptive message referencing the rationale\n\
         5. Push the branch\n\
         6. Open a PR targeting the default branch with `gh pr create`\n\n\
         On the last line of your output, print PR_URL=<full PR URL>"
    )
}
