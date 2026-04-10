//! Review phase prompts.

use super::helpers::{shell_single_quote, wrap_external_data};

/// Return project-type-specific review focus bullet points.
fn review_focus_for_type(project_type: &str) -> &'static str {
    match project_type {
        "rust" => "- Concurrency: race conditions, deadlocks, shared mutable state, Mutex/RwLock misuse\n\
                   - Error handling: Result/Option misuse, silent unwrap panics, missing ? propagation",
        "shell" => "- Quoting: unquoted variables, word-splitting, glob expansion in unsafe contexts\n\
                    - Portability: bash-isms in /bin/sh scripts, non-POSIX constructs\n\
                    - Injection: unsanitised user input passed to eval or command substitution",
        "documentation" => "- Accuracy: code examples must compile and match the described behaviour\n\
                            - Broken links: internal anchors and external URLs must resolve\n\
                            - Completeness: all public APIs documented, no missing parameters or return values",
        _ => "- Concurrency: race conditions, shared mutable state\n\
              - Error handling: silent failures, unhandled error paths",
    }
}

/// Return the validation command appropriate for the project type.
fn validation_cmd_for_type(project_type: &str) -> &'static str {
    match project_type {
        "rust" => "cargo check and cargo test",
        "shell" => "shellcheck on changed scripts and bash -n for syntax checks",
        "documentation" => "link checker and verify all referenced code examples are correct",
        _ => "project validation checks",
    }
}

/// Return project-type-specific P1 Logic review criteria.
fn p1_logic_for_type(project_type: &str) -> &'static str {
    match project_type {
        "rust" => "deadlocks, race conditions, declaration-execution gaps, error handling",
        "shell" => "unquoted variables, command injection, missing error checks (set -e/pipefail)",
        "documentation" => "accuracy against source code, broken links, completeness",
        _ => "logic errors, declaration-execution gaps, error handling",
    }
}

/// Build review loop prompt for a given round.
///
/// `round` controls convergence behavior:
/// - Round 2: fix all critical/high/medium comments
/// - Round 3+: only fix critical/high; skip medium style/design suggestions
///
/// When `prev_fixed` is true (previous round pushed code), the agent must first
/// verify that Gemini has submitted a **new** review covering the latest commit
/// before declaring LGTM. If no new review exists yet, agent outputs WAITING.
#[allow(clippy::too_many_arguments)]
pub fn review_prompt(
    issue: Option<u64>,
    pr: u64,
    round: u32,
    prev_fixed: bool,
    review_bot_command: &str,
    reviewer_name: &str,
    repo: &str,
    impasse: bool,
) -> String {
    let context = match issue {
        Some(n) => format!("You previously created PR #{pr} for issue #{n}.\n"),
        None => format!("Review PR #{pr}.\n"),
    };

    let severity_guidance = if impasse {
        "IMPASSE MODE: Only fix critical severity issues that block merge \
         (security vulnerabilities, data loss, build failures). \
         Skip ALL other review comments — they will be addressed in a follow-up PR."
    } else if round <= 2 {
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
             1. Run `gh api repos/{repo}/pulls/{pr}/reviews \
             --jq '{login_filter}'` \
             to get the timestamp of {reviewer_name}'s most recent review\n\
             2. Run `gh api repos/{repo}/pulls/{pr}/commits --jq '.[-1].commit.committer.date'` \
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
         1. Run `gh pr view {pr} --json statusCheckRollup` and parse the JSON. \
         CI passes only if the `state` field in the `statusCheckRollup` object is `SUCCESS`\n\
         2. Run `gh api repos/{repo}/pulls/{pr}/reviews` to read review verdicts\n\
         3. Run `gh api repos/{repo}/pulls/{pr}/comments` to read inline review comments\n\
         4. {severity_guidance}\n\
         5. If all CI checks pass and there are no unresolved review comments \
         that match the severity criteria above, print LGTM on the last line\n\
         6. Otherwise fix the issues, {push_action}, \
         and print FIXED on the last line\
         {freshness_check}\n\n\
         Output format (always include both lines at the end):\n\
         - `ISSUES=<number>` — total unresolved review comments before your fixes\n\
         - Then `LGTM`, `FIXED`, or `WAITING` on the very last line\n\n\
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
pub fn agent_review_prompt(pr_url: &str, round: u32, project_type: &str) -> String {
    let focus = review_focus_for_type(project_type);
    format!(
        "You are a Staff Engineer conducting an independent code review of PR {pr_url} \
         (review round {round}).\n\n\
         Your job is to find bugs that pass CI but blow up in production. Think about:\n\
         - Error paths: what happens when this fails? Is the failure handled or silently swallowed?\n\
         - Edge cases: empty inputs, large inputs, unicode, off-by-one\n\
         - Security: injection, path traversal, unvalidated input at system boundaries\n\
         {focus}\n\n\
         Steps:\n\
         1. Run `gh pr diff '{pr_url}'` to read the full diff\n\
         2. For each changed file, understand the CONTEXT — read surrounding code if needed\n\
         3. Evaluate correctness and safety in priority order: security > logic > quality > style\n\
         4. If everything looks good, print APPROVED on the last line\n\
         5. Otherwise, list each issue on its own line prefixed with \"ISSUE: \"\n\n\
         Constraints:\n\
         - Focus on correctness and safety, not cosmetic preferences\n\
         - NEVER downgrade dependency versions\n\
         - Be specific: reference file names and line numbers\n\
         - Do NOT flag style preferences as issues (naming, formatting, comment density)"
    )
}

/// Build prompt: implementor fixes issues found by the reviewer agent.
pub fn agent_review_fix_prompt(
    pr_url: &str,
    issues: &[String],
    round: u32,
    project_type: &str,
) -> String {
    let issue_list: String = issues
        .iter()
        .enumerate()
        .map(|(i, issue)| format!("{}. {issue}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let safe_issue_list = wrap_external_data(&issue_list);
    let validation_cmd = validation_cmd_for_type(project_type);
    format!(
        "The independent reviewer found the following issues in PR {pr_url} \
         (agent review round {round}):\n\n{safe_issue_list}\n\n\
         Fix each issue, run {validation_cmd}, then commit and push.\n\
         On the last line of your output, print PR_URL=<PR URL>"
    )
}

/// Build prompt: impasse detected — same issues repeated from the previous round.
///
/// Tells the implementor that their previous fix did not resolve the flagged issues
/// and asks them to try a different approach rather than repeating the same strategy.
pub fn agent_review_intervention_prompt(
    pr_url: &str,
    issues: &[String],
    round: u32,
    project_type: &str,
) -> String {
    let issue_list: String = issues
        .iter()
        .enumerate()
        .map(|(i, issue)| format!("{}. {issue}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let safe_issue_list = wrap_external_data(&issue_list);
    let validation_cmd = validation_cmd_for_type(project_type);
    format!(
        "IMPASSE DETECTED: The previous fix attempt did not resolve the issues in PR {pr_url} \
         (agent review round {round}). The reviewer flagged the same problems again.\n\n\
         Re-examine root causes rather than symptoms. Consider a different approach entirely \
         instead of repeating the same fix strategy.\n\n\
         Issues that remain unresolved:\n\n{safe_issue_list}\n\n\
         Fix each issue, run {validation_cmd}, then commit and push.\n\
         On the last line of your output, print PR_URL=<PR URL>"
    )
}

/// Build prompt: synthesize two independent reviews into a final verdict.
///
/// The agent receives both raw reviews and decides what's real, what's noise,
/// and what the final report should contain. No pre-classification framework.
pub fn review_synthesis_prompt_with_agents(
    left_agent: &str,
    left_review: &str,
    right_agent: &str,
    right_review: &str,
) -> String {
    let safe_left = wrap_external_data(left_review);
    let safe_right = wrap_external_data(right_review);
    format!(
        "You are a Staff Engineer. Two independent reviewers ({left_agent} and {right_agent}) just reviewed the same codebase.\n\n\
         ## {left_agent}'s Review\n{safe_left}\n\n\
         ## {right_agent}'s Review\n{safe_right}\n\n\
         Read both reviews. Use your own judgment to produce the final report:\n\
         - Read the actual code to verify any finding you're unsure about\n\
         - Findings both reviewers agree on are likely real\n\
         - Findings only one reviewer caught may still be valid — check the code\n\
         - Discard anything that's wrong or not actionable\n\
         - Create GitHub issues for P0/P1 findings (`gh issue list --state open --label review` first to avoid duplicates)\n\n\
         Output the final REVIEW_JSON between markers:\n\n\
         REVIEW_JSON_START\n\
         {{ \"findings\": [...], \"summary\": {{ \"p0_count\": N, \"p1_count\": N, \"p2_count\": N, \"p3_count\": N, \"health_score\": N }} }}\n\
         REVIEW_JSON_END\n\n\
         health_score = 100 minus deductions (P0: -15, P1: -8, P2: -3, P3: -1), minimum 0."
    )
}

pub fn review_synthesis_prompt(claude_review: &str, codex_review: &str) -> String {
    review_synthesis_prompt_with_agents("Claude", claude_review, "Codex", codex_review)
}

/// Build prompt: periodic codebase review.
///
/// The agent receives only the project path and explores the codebase itself
/// using its tools (read files, run commands, `gh issue list`). No pre-chewed
/// data is stuffed into the prompt — the agent decides what to look at.
pub fn periodic_review_prompt(project_root: &str, since: &str, project_type: &str) -> String {
    periodic_review_prompt_with_guard_scan(project_root, since, project_type, None)
}

/// Build prompt: periodic codebase review, with optional pre-scanned guard results.
///
/// When `guard_scan` is `Some`, the guard scan was already run on the source
/// repo (`project_root`) by the scheduler — the agent must NOT re-run guard
/// scripts in its worktree, which may contain nested `.harness/worktrees/`
/// from previous runs that would inflate the violation count ~3x.
pub fn periodic_review_prompt_with_guard_scan(
    project_root: &str,
    since: &str,
    project_type: &str,
    guard_scan: Option<&str>,
) -> String {
    let p1_logic = p1_logic_for_type(project_type);

    let guard_section = match guard_scan {
        Some(scan_output) => {
            let safe_scan = wrap_external_data(scan_output);
            format!(
                "## Guard Scan Results (pre-scanned on source repo)\n\n\
                 The scheduler already ran guard scripts on `{project_root}` before\n\
                 creating this task. Do NOT re-run guard scripts — the worktree may\n\
                 contain nested workspace directories that inflate results.\n\n\
                 {safe_scan}\n\n"
            )
        }
        None => String::new(),
    };

    let guard_step = if guard_scan.is_some() {
        "4. Guard scan results are provided above — do NOT re-run guard scripts\n"
    } else {
        "4. Run guard scripts if they exist: `bash .vibeguard/run-guards.sh` or similar\n"
    };

    format!(
        "You are a Staff Engineer conducting a periodic health review of this project.\n\n\
         Project: {project_root}\n\
         Last review: {since}\n\
         Project type: {project_type}\n\n\
         {guard_section}\
         ## Steps\n\n\
         0. Run `git log --oneline --since=\"{since}\"`. If the output is empty (no commits \
since last review), output exactly `REVIEW_SKIPPED` on its own line and stop — do not \
create any issues or output JSON.\n\
         1. Run `git log --oneline --since=\"{since}\"` to see what changed\n\
         2. Run `git diff --stat HEAD~20` (or since last review) to identify changed files\n\
         3. Read the changed files — focus on correctness and safety, not style\n\
         {guard_step}\
         5. Check existing issues: `gh issue list --state open --label review` — do NOT duplicate\n\
         6. For P0 (security) and P1 (logic) findings, create GitHub issues with `gh issue create`\n\n\
         ## Priority\n\n\
         P0 Security: injection, path traversal, hardcoded secrets, unauth endpoints\n\
         P1 Logic: {p1_logic}\n\
         P2 Quality: oversized files, dead code, duplicate types\n\
         P3 Performance: unnecessary allocations, dependency bloat\n\n\
         ## Issue format\n\n\
         - Title: `[PRIORITY] RULE_ID: short title`\n\
         - Body: description + recommended action + file:line reference\n\
         - Labels: `review`, priority label (`p0`, `p1`)\n\
         - Only create issues for P0 and P1. Do NOT create PRs or edit files.\n\n\
         ## Output\n\n\
         After creating issues, output structured JSON between markers:\n\n\
         REVIEW_JSON_START\n\
         {{\n\
           \"findings\": [\n\
             {{\n\
               \"id\": \"F001\",\n\
               \"rule_id\": \"SEC-07\",\n\
               \"priority\": \"P0\",\n\
               \"impact\": 5,\n\
               \"confidence\": 4,\n\
               \"effort\": 2,\n\
               \"file\": \"crates/example/src/lib.rs\",\n\
               \"line\": 42,\n\
               \"title\": \"Short descriptive title\",\n\
               \"description\": \"What the issue is and why it matters\",\n\
               \"action\": \"Specific fix recommendation\"\n\
             }}\n\
           ],\n\
           \"summary\": {{\n\
             \"p0_count\": 0,\n\
             \"p1_count\": 0,\n\
             \"p2_count\": 0,\n\
             \"p3_count\": 0,\n\
             \"health_score\": 75\n\
           }}\n\
         }}\n\
         REVIEW_JSON_END\n\n\
         health_score = 100 minus deductions (P0: -15, P1: -8, P2: -3, P3: -1), minimum 0.\n\
         The JSON must be valid and parseable. No markdown fences inside the markers."
    )
}
