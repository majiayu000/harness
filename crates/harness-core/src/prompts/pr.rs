use super::shell_single_quote;
use std::path::Path;

/// Build prompt: continue work on an existing PR for a GitHub issue.
///
/// Used when a prior task already created a PR for this issue. Instead of
/// creating a duplicate PR, the agent checks out the existing branch, reads
/// review feedback, continues the implementation, and pushes to the same branch.
pub fn continue_existing_pr(issue: u64, pr_number: u64, branch: &str, repo: &str) -> String {
    format!(
        "GitHub issue #{issue} already has an open PR #{pr_number} on branch `{branch}`.\n\n\
         IMPORTANT: Never run `git checkout` or `git stash` in the main repository working tree.\n\
         All work must be done in an isolated worktree.\n\n\
         Steps:\n\
         1. Create an isolated worktree:\n\
            ```\n\
            git fetch origin {branch}\n\
            git worktree remove /tmp/harness-pr-{pr_number} 2>/dev/null || rm -rf /tmp/harness-pr-{pr_number} 2>/dev/null || true\n\
            git worktree prune\n\
            git worktree add /tmp/harness-pr-{pr_number} {branch}\n\
            ```\n\
         2. Read the PR diff and any review comments:\n\
            - `gh pr diff {pr_number}`\n\
            - `gh api repos/{repo}/pulls/{pr_number}/comments`\n\
            - `gh api repos/{repo}/pulls/{pr_number}/reviews`\n\
         3. Read the original issue requirements: `gh issue view {issue}`\n\
         4. Fix any unresolved review comments and continue the implementation if incomplete.\n\
            All editing must happen inside `/tmp/harness-pr-{pr_number}`.\n\
         5. Run the project's validation commands inside `/tmp/harness-pr-{pr_number}` and fix any failures before pushing.\n\
         6. Commit and push from the worktree to the SAME branch `{branch}` — do NOT create a new PR:\n\
            `cd /tmp/harness-pr-{pr_number} && git push origin {branch}`\n\
         7. Clean up: `git worktree remove /tmp/harness-pr-{pr_number}`\n\n\
         On the last line of your output, print PR_URL=https://github.com/{repo}/pull/{pr_number}"
    )
}

/// Build prompt: rebase a conflicting PR onto the current main branch.
///
/// Used when [`conflict_resolver::assess_pr_conflict`] classifies the PR as
/// `Small` (≤3 files, <5 conflict regions). The agent performs the rebase
/// inside an isolated worktree and force-pushes the result.
///
/// `project_root` is used to detect the project language so the validation
/// step uses the correct build/test toolchain instead of hardcoding `cargo`.
pub fn rebase_conflicting_pr(pr_num: u64, branch: &str, repo: &str, project_root: &Path) -> String {
    let validation_step = rebase_validation_step(pr_num, project_root);
    let branch_context = if branch.trim().is_empty() {
        format!(
            "PR #{pr_num} in `{repo}` has a merge conflict that is small enough for automatic rebase.\n\
             Harness did not inspect GitHub locally. First determine the PR head branch from GitHub, then use that exact branch name for the commands below."
        )
    } else {
        format!(
            "PR #{pr_num} on branch `{branch}` in `{repo}` has a merge conflict that is small enough for automatic rebase."
        )
    };
    let branch_arg = if branch.trim().is_empty() {
        "<pr-head-branch>"
    } else {
        branch
    };
    format!(
        "{branch_context}\n\n\
         IMPORTANT: Never run `git checkout` or `git stash` in the main repository working tree.\n\
         All work must be done in an isolated worktree.\n\n\
         Steps:\n\
         1. Fetch and create an isolated worktree:\n\
            ```\n\
            git fetch origin\n\
            git worktree remove /tmp/harness-rebase-{pr_num} 2>/dev/null || rm -rf /tmp/harness-rebase-{pr_num} 2>/dev/null || true\n\
            git worktree prune\n\
            git worktree add /tmp/harness-rebase-{pr_num} '{branch_arg}'\n\
            ```\n\
         2. Rebase onto origin/main inside the worktree:\n\
            ```\n\
            cd /tmp/harness-rebase-{pr_num}\n\
            git rebase origin/main\n\
            ```\n\
         3. If rebase conflicts appear, resolve each file, then:\n\
            ```\n\
            git add <resolved-files>\n\
            git rebase --continue\n\
            ```\n\
            Repeat until the rebase completes successfully.\n\
         {validation_step}\
         5. Force-push the rebased branch:\n\
            ```\n\
            git push --force-with-lease origin '{branch_arg}'\n\
            ```\n\
         6. Clean up: `git worktree remove /tmp/harness-rebase-{pr_num}`\n\n\
         On the last line of your output, print exactly one of:\n\
         - `REBASE_OK` if the rebase and push succeeded.\n\
         - `REBASE_FAILED` if you could not complete the rebase for any reason."
    )
}

/// Build step 4 of the rebase prompt: language-appropriate validation commands.
///
/// Detects the project toolchain from `project_root` and emits the correct
/// build/test commands. Falls back to a prose instruction for unrecognised
/// project types so the agent can decide what to run rather than invoking the
/// wrong toolchain (e.g. `cargo` in a TypeScript project).
fn rebase_validation_step(pr_num: u64, project_root: &Path) -> String {
    use crate::lang_detect::{default_pre_push_commands, detect_language};

    let lang = detect_language(project_root);
    // Use only pre-push (non-mutating build+test) commands. Pre-commit commands
    // include mutating formatters (cargo fmt --all, gofmt -w) that modify the
    // working tree without committing — pushing after those would leave CI
    // formatting gates unsatisfied on the actual committed code.
    let cmds = default_pre_push_commands(lang, project_root);

    if cmds.is_empty() {
        // Unknown project type — give a prose instruction so the agent runs
        // whatever the project provides instead of a wrong hardcoded command.
        "4. Verify the result is correct before pushing using the project's available \
build and test tools. If verification fails or you cannot determine how to validate, \
print `REBASE_FAILED` and stop — do NOT force-push.\n         "
            .to_string()
    } else {
        let cmd_str = cmds.join(" && ");
        format!(
            "4. Verify the result compiles and tests pass before pushing:\n\
            ```\n\
            cd /tmp/harness-rebase-{pr_num}\n\
            {cmd_str}\n\
            ```\n\
            If this step fails, print `REBASE_FAILED` and stop — do NOT force-push.\n\
         "
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_prompt_contains_force_push() {
        let p = rebase_conflicting_pr(
            42,
            "feat/my-branch",
            "owner/repo",
            std::path::Path::new("."),
        );
        assert!(
            p.contains("--force-with-lease"),
            "prompt must instruct force-with-lease push"
        );
    }

    #[test]
    fn rebase_prompt_contains_pr_branch() {
        let branch = "fix/issue-123";
        let p = rebase_conflicting_pr(123, branch, "owner/repo", std::path::Path::new("."));
        assert!(p.contains(branch), "prompt must reference the PR branch");
    }

    #[test]
    fn rebase_prompt_validation_step_is_language_aware() {
        use std::fs;
        let dir = tempfile::tempdir().expect("tempdir");
        // Rust project
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"t\"").unwrap();
        let p = rebase_conflicting_pr(1, "b", "o/r", dir.path());
        assert!(
            p.contains("cargo"),
            "Rust project must use cargo for validation"
        );
        assert!(!p.contains("npm"), "Rust project must not reference npm");

        // TypeScript project
        let dir2 = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir2.path().join("package.json"),
            r#"{"scripts":{"test":"echo ok"}}"#,
        )
        .unwrap();
        let p2 = rebase_conflicting_pr(2, "b", "o/r", dir2.path());
        assert!(!p2.contains("cargo"), "TS project must not use cargo");

        // Unknown project — should not contain any specific toolchain command
        let dir3 = tempfile::tempdir().expect("tempdir");
        let p3 = rebase_conflicting_pr(3, "b", "o/r", dir3.path());
        assert!(!p3.contains("cargo"), "unknown project must not use cargo");
        assert!(
            !p3.contains("npm"),
            "unknown project must not reference npm"
        );
    }

    #[test]
    fn test_continue_existing_pr() {
        let p = continue_existing_pr(29, 50, "fix/issue-29", "owner/repo");
        assert!(p.contains("issue #29"));
        assert!(p.contains("PR #50"));
        assert!(p.contains("fix/issue-29"));
        assert!(p.contains("do NOT create a new PR"));
        assert!(p.contains("validation commands"));
        assert!(p.contains("PR_URL=https://github.com/owner/repo/pull/50"));
        assert!(p.contains("repos/owner/repo/pulls/50/comments"));
        // Worktree isolation: must use worktree, must not bare-checkout in main repo
        assert!(p.contains("worktree add /tmp/harness-pr-50"));
        assert!(p.contains("worktree remove /tmp/harness-pr-50"));
        assert!(!p.contains("git checkout fix/issue-29"));
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
    fn test_check_existing_pr_shell_quoting() {
        // A command containing a single quote must not break single-quoting
        let p = check_existing_pr(5, "it's a test", "owner/repo", "bot[bot]", false);
        assert!(
            p.contains(r"'it'\''s a test'"),
            "single quote must be escaped"
        );
    }
}
