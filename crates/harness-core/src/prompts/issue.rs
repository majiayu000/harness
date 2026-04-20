use super::PromptParts;
use crate::config::project::GitConfig;

/// Wrap user-supplied content in delimiters to separate it from trusted instructions.
/// Escapes the closing tag within content to prevent delimiter injection.
pub fn wrap_external_data(content: &str) -> String {
    let escaped = content.replace("</external_data>", "<\\/external_data>");
    format!("<external_data>\n{}\n</external_data>", escaped)
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
             **Step 1 — duplicate PR check:**\n\
             Run: `gh pr list --search \"{issue}\" --state open --json number,title`\n\
             If an open PR targets this issue, mention the PR number in your response, then follow the mandatory output format below with TRIAGE=SKIP.\n\n\
             **Step 2 — assess the issue (if no duplicate PR):**\n\
             Read the issue. Assess complexity and clarity briefly (2-3 sentences).\n\n\
             **Step 3 — choose ONE recommendation:**\n\
             - PROCEED — clear change, go straight to implementation\n\
             - PROCEED_WITH_PLAN — complex change, needs a plan first\n\
             - SKIP — ONLY if the issue is a literal duplicate of another issue, or fundamentally impossible\n\n\
             **IMPORTANT RULES:**\n\
             - Default to PROCEED or PROCEED_WITH_PLAN. Most issues should be attempted.\n\
             - Do NOT skip issues just because they are large, complex, or ambitious.\n\
             - Do NOT use NEEDS_CLARIFICATION — if the issue is unclear, PROCEED_WITH_PLAN and let the planner figure it out.\n\
             - Large refactors (4+ files) → PROCEED_WITH_PLAN, never SKIP.\n\n\
             **OUTPUT FORMAT (mandatory, must be the last 2 lines):**\n\
             COMPLEXITY=<low|medium|high>\n\
             TRIAGE=<PROCEED|PROCEED_WITH_PLAN|SKIP>"
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
             6. Draft the PR body in `/tmp/pr_body.md`. Follow \
`.github/pull_request_template.md` (Summary + Test plan sections) and end \
with a line reading exactly `Closes #{issue}` so GitHub auto-closes the issue \
on merge.\n\
             7. Validate the body before publishing: \
`scripts/pr_body_check.sh --file /tmp/pr_body.md --issue {issue}` — this \
must exit 0. Fix the body and re-run until it passes.\n\
             8. Publish with `gh pr create --body-file /tmp/pr_body.md` (do NOT \
use `--body`; the validator only guards the file-backed path)."
        ),
        context: git_line,
        dynamic_payload: "On the last line of your output, print PR_URL=<full PR URL>".to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_adopt_prompt() {
        let p = gc_adopt_prompt(
            "abc123",
            "Auto-generated fix for RepeatedWarn signal",
            "Run guard check after applying",
            &[".harness/drafts/abc123.md"],
        );
        assert!(p.contains("gc/abc123"), "should include branch name");
        assert!(p.contains("Auto-generated fix"), "should include rationale");
        assert!(p.contains("guard check"), "should include validation");
        assert!(
            p.contains(".harness/drafts/abc123.md"),
            "should include path"
        );
        assert!(p.contains("PR_URL="), "should include PR_URL instruction");
        assert!(
            p.contains("gh pr create"),
            "should include pr create command"
        );
    }

    #[test]
    fn test_gc_adopt_prompt_escapes_closing_tag_in_rationale() {
        let p = gc_adopt_prompt(
            "id1",
            "rationale with </external_data> injection",
            "validate",
            &["file.md"],
        );
        assert!(
            !p.contains("</external_data>\nRationale"),
            "closing tag must be escaped"
        );
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
        use crate::config::project::GitConfig;
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
    fn test_implement_from_prompt() {
        let p = implement_from_prompt("fix the bug", None);
        assert!(p.contains("fix the bug"));
        assert!(p.contains("PR_URL="));
    }

    // --- Triage / Plan phase tests ---

    #[test]
    fn test_triage_prompt_contains_issue_and_recommendations() {
        let parts = triage_prompt(42);
        let s = parts.to_prompt_string();
        assert!(s.contains("issue #42"));
        assert!(s.contains("Tech Lead"));
        assert!(s.contains("PROCEED"));
        assert!(s.contains("PROCEED_WITH_PLAN"));
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
