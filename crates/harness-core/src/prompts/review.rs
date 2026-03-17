//! Prompts and output parsers for all review-related agent workflows.
//!
//! Covers the PR review loop, agent peer-review, incremental periodic review,
//! and full-repo periodic review.

use super::{last_non_empty_line, shell_single_quote, wrap_external_data};

/// Build review loop prompt for a given round.
///
/// `round` controls convergence behavior:
/// - Round 2: fix all critical/high/medium comments
/// - Round 3+: only fix critical/high; skip medium style/design suggestions
///
/// When `prev_fixed` is true (previous round pushed code), the agent must first
/// verify that Gemini has submitted a **new** review covering the latest commit
/// before declaring LGTM. If no new review exists yet, agent outputs WAITING.
pub fn review_prompt(
    issue: Option<u64>,
    pr: u64,
    round: u32,
    prev_fixed: bool,
    review_bot_command: &str,
    reviewer_name: &str,
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

    let body = shell_single_quote(review_bot_command);
    let push_action = format!(
        "commit, push, then run `gh pr comment {pr} --body {body}` \
         to trigger re-review on the new code"
    );

    let freshness_check = if prev_fixed {
        // Filter to reviews authored by the configured bot login so that a human
        // reviewer submitting after the latest commit cannot be mistaken for the
        // bot's re-review.
        let login_filter =
            format!("[.[] | select(.user.login == \"{reviewer_name}\")] | last | .submitted_at");
        format!(
            "\n\nIMPORTANT — New review verification:\n\
             The previous round pushed a fix commit. Before evaluating review status, \
             you MUST verify that {reviewer_name} has submitted a NEW review covering the latest commit:\n\
             1. Run `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr}/reviews \
             --jq '{login_filter}'` \
             to get the timestamp of {reviewer_name}'s most recent review\n\
             2. Run `gh api repos/{{{{owner}}}}/{{{{repo}}}}/pulls/{pr}/commits --jq '.[-1].commit.committer.date'` \
             to get the timestamp of the latest commit\n\
             3. If {reviewer_name}'s latest review was submitted BEFORE the latest commit \
             (or no review from {reviewer_name} exists), \
             {reviewer_name} has not yet re-reviewed the new code. \
             In this case, print WAITING on the last line and stop.\n\
             4. Only proceed with the review evaluation below if {reviewer_name}'s latest review \
             was submitted AFTER the latest commit."
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

/// Build prompt: reviewer agent evaluates a PR diff.
///
/// The reviewer reads the diff and outputs either `APPROVED` on the last line
/// or lists issues prefixed with `ISSUE:`.
pub fn agent_review_prompt(pr: u64, round: u32) -> String {
    format!(
        "You are an independent code reviewer. Review PR #{pr} (agent review round {round}).\n\n\
         Steps:\n\
         1. Run `gh pr diff {pr}` to read the full diff\n\
         2. Check for correctness, safety, and style issues\n\
         3. If everything looks good, print APPROVED on the last line\n\
         4. Otherwise, list each issue on its own line prefixed with \"ISSUE: \"\n\n\
         Constraints:\n\
         - Focus on correctness and safety, not cosmetic preferences\n\
         - NEVER downgrade dependency versions\n\
         - Be specific: reference file names and line numbers"
    )
}

/// Build prompt: implementor fixes issues found by the reviewer agent.
pub fn agent_review_fix_prompt(pr: u64, issues: &[String], round: u32) -> String {
    let issue_list: String = issues
        .iter()
        .enumerate()
        .map(|(i, issue)| format!("{}. {issue}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let safe_issue_list = wrap_external_data(&issue_list);
    format!(
        "The independent reviewer found the following issues in PR #{pr} \
         (agent review round {round}):\n\n{safe_issue_list}\n\n\
         Fix each issue, run cargo check and cargo test, then commit and push.\n\
         On the last line of your output, print PR_URL=<PR URL>"
    )
}

/// Build prompt: periodic codebase review with an 11-item checklist.
///
/// `repo_structure` is the output of a directory listing (e.g., `find . -type f -name '*.rs'`).
/// `diff_stat` is the output of `git diff --stat` since the last review.
/// `recent_commits` is the output of `git log --oneline` since the last review.
pub fn periodic_review_prompt(
    repo_structure: &str,
    diff_stat: &str,
    recent_commits: &str,
) -> String {
    let safe_structure = wrap_external_data(repo_structure);
    let safe_diff_stat = wrap_external_data(diff_stat);
    let safe_commits = wrap_external_data(recent_commits);
    format!(
        "You are conducting a periodic codebase health review. \
         Examine the entire codebase for the 11 categories of issues below. \
         Produce a structured markdown report with severity-ranked findings.\n\n\
         ## Context\n\n\
         Repository structure:\n{safe_structure}\n\n\
         Changes since last review (diff stat):\n{safe_diff_stat}\n\n\
         Recent commits:\n{safe_commits}\n\n\
         ## Review Checklist\n\n\
         Check for ALL of the following (mark each item even if no issues found):\n\n\
         ### CRITICAL\n\
         1. **Duplicate Type Definitions** — Structs or enums with the same name in multiple \
         crates. Config structs duplicated across crate boundaries.\n\
         7. **Declaration-Execution Gap** — Components built but never wired into the actual \
         execution path. Modules registered in lib.rs but never called from startup/runtime code. \
         (Note: config structs using Default::default() instead of loaded values belong to #11, \
         not here.)\n\n\
         ### HIGH\n\
         2. **Oversized Files** — Any .rs file exceeding 400 lines. Report exact line count \
         and suggest split points.\n\
         3. **God Objects** — Structs with more than 10 public fields. Modules mixing unrelated concerns.\n\
         8. **Dead Code** — pub functions with zero call sites outside their own module. \
         Structs or enums defined but never instantiated. Entire modules exported via pub mod \
         but never imported. (Exclude #[cfg(test)] code.)\n\
         10. **Project Rule Violations** — Verify CLAUDE.md rules: ZERO Command::new(\"gh\") or \
         Command::new(\"git\") calls (all git/GitHub interaction must be in agent prompts only); \
         all user-facing strings and comments in English; no hardcoded ports/URLs/credentials; \
         cargo fmt compliance.\n\n\
         ### MEDIUM\n\
         4. **Public API Leakage** — lib.rs files exporting more than 5 pub mod entries. \
         Internal implementation details exposed as public.\n\
         5. **Repeated Patterns** — Same function signature pattern appearing 3+ times across \
         files. Boilerplate that should be abstracted.\n\
         9. **Error Handling Inconsistency** — Mixing anyhow::Result and custom error types \
         without clear boundary rules. Silently discarding meaningful errors with let _ or .ok(). \
         .unwrap() in non-test async code.\n\
         11. **Config-Default Divergence** — Config struct has fields with serde defaults, but \
         consuming code constructs via Default::default() instead of loading from file.\n\n\
         ### LOW\n\
         6. **Dependency Issues** — Crates depending on more than 5 workspace siblings. \
         Circular or unnecessary dependencies.\n\n\
         ## Output Format\n\n\
         For each finding:\n\
         ```\n\
         ## [SEVERITY] Category: Short Title\n\n\
         **File:** path/to/file.rs:LINE\n\
         **Details:** What the issue is and why it matters\n\
         **Action:** Specific fix recommendation\n\
         ```\n\n\
         End with a summary table:\n\
         ```\n\
         | Severity | Count |\n\
         |----------|-------|\n\
         | CRITICAL | N     |\n\
         | HIGH     | N     |\n\
         | MEDIUM   | N     |\n\
         | LOW      | N     |\n\
         ```\n\n\
         If a category has no findings, include a one-line note: \
         `## [SEVERITY] Category: No issues found.`"
    )
}

/// Full-repo review prompt for `mode = "full"`.
///
/// Unlike the incremental variant, this prompt contains no diff or commit context so the
/// agent examines the entire codebase from scratch each cycle.
pub fn full_repo_review_prompt(repo_structure: &str) -> String {
    let safe_structure = wrap_external_data(repo_structure);
    format!(
        "You are conducting a full periodic codebase health review. \
         Examine the entire codebase for the 11 categories of issues below. \
         Produce a structured markdown report with severity-ranked findings.\n\n\
         ## Context\n\n\
         Repository structure:\n{safe_structure}\n\n\
         ## Review Checklist\n\n\
         Check for ALL of the following (mark each item even if no issues found):\n\n\
         ### CRITICAL\n\
         1. **Duplicate Type Definitions** — Structs or enums with the same name in multiple \
         crates. Config structs duplicated across crate boundaries.\n\
         7. **Declaration-Execution Gap** — Components built but never wired into the actual \
         execution path. Modules registered in lib.rs but never called from startup/runtime code. \
         (Note: config structs using Default::default() instead of loaded values belong to #11, \
         not here.)\n\n\
         ### HIGH\n\
         2. **Oversized Files** — Any .rs file exceeding 400 lines. Report exact line count \
         and suggest split points.\n\
         3. **God Objects** — Structs with more than 10 public fields. Modules mixing unrelated concerns.\n\
         8. **Dead Code** — pub functions with zero call sites outside their own module. \
         Structs or enums defined but never instantiated. Entire modules exported via pub mod \
         but never imported. (Exclude #[cfg(test)] code.)\n\
         10. **Project Rule Violations** — Verify CLAUDE.md rules: ZERO Command::new(\"gh\") or \
         Command::new(\"git\") calls (all git/GitHub interaction must be in agent prompts only); \
         all user-facing strings and comments in English; no hardcoded ports/URLs/credentials; \
         cargo fmt compliance.\n\n\
         ### MEDIUM\n\
         4. **Public API Leakage** — lib.rs files exporting more than 5 pub mod entries. \
         Internal implementation details exposed as public.\n\
         5. **Repeated Patterns** — Same function signature pattern appearing 3+ times across \
         files. Boilerplate that should be abstracted.\n\
         9. **Error Handling Inconsistency** — Mixing anyhow::Result and custom error types \
         without clear boundary rules. Silently discarding meaningful errors with let _ or .ok(). \
         .unwrap() in non-test async code.\n\
         11. **Config-Default Divergence** — Config struct has fields with serde defaults, but \
         consuming code constructs via Default::default() instead of loading from file.\n\n\
         ### LOW\n\
         6. **Dependency Issues** — Crates depending on more than 5 workspace siblings. \
         Circular or unnecessary dependencies.\n\n\
         ## Output Format\n\n\
         For each finding:\n\
         ```\n\
         ## [SEVERITY] Category: Short Title\n\n\
         **File:** path/to/file.rs:LINE\n\
         **Details:** What the issue is and why it matters\n\
         **Action:** Specific fix recommendation\n\
         ```\n\n\
         End with a summary table:\n\
         ```\n\
         | Severity | Count |\n\
         |----------|-------|\n\
         | CRITICAL | N     |\n\
         | HIGH     | N     |\n\
         | MEDIUM   | N     |\n\
         | LOW      | N     |\n\
         ```\n\n\
         If a category has no findings, include a one-line note: \
         `## [SEVERITY] Category: No issues found.`"
    )
}

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
