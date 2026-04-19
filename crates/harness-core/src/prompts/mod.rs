//! Prompt templates and output parsers shared across CLI and HTTP entries.

pub mod context;
pub mod contract;
pub mod issue;
pub mod parsing;
pub mod pr;
pub mod review;

// Re-export all public symbols so callers using `crate::prompts::foo` are unaffected.
pub use context::{
    build_available_skills_listing, build_matched_skills_section, parse_sprint_plan,
    sibling_task_context, sprint_plan_prompt, SiblingTask, SprintPlan, SprintSkip, SprintTask,
};
pub use contract::{
    agent_review_capability_note, evaluator_prompt, plan_for_prompt_task, sprint_contract_prompt,
    test_gate_failure_prompt, validation_retry_prompt,
};
pub use issue::{
    gc_adopt_prompt, implement_from_issue, implement_from_prompt, plan_prompt, triage_prompt,
    wrap_external_data,
};
pub use parsing::{
    extract_pr_number, extract_review_issues, is_lgtm, is_quota_exhausted, is_waiting,
    parse_complexity, parse_created_issue_number, parse_github_pr_url, parse_issue_count,
    parse_plan_issue, parse_pr_url, parse_triage, repo_slug_from_pr_url, TriageComplexity,
    TriageDecision,
};
pub use pr::{check_existing_pr, continue_existing_pr, rebase_conflicting_pr};
pub use review::{
    agent_review_fix_prompt, agent_review_intervention_prompt, agent_review_prompt, is_approved,
    periodic_review_prompt, periodic_review_prompt_with_guard_scan, review_prompt,
    review_synthesis_prompt, review_synthesis_prompt_with_agents,
};

/// A prompt decomposed into its static, semi-static, and dynamic layers.
///
/// This structure prepares prompts for future API-level prompt caching by separating
/// stable content (instructions, output format) from dynamic content (issue body, diff).
///
/// ## Layers
/// - `static_instructions`: Role description, workflow steps, output format — identical
///   across all tasks of the same type. Highest cache hit rate.
/// - `context`: Project-level configuration (git config, rules, sibling tasks) — stable
///   within a session but varies per project.
/// - `dynamic_payload`: Issue body, PR diff, review comments — unique per invocation.
///
/// Call [`to_prompt_string`](PromptParts::to_prompt_string) to concatenate all parts into
/// the final prompt string. The result is identical to what the previous `String`-returning
/// functions produced.
pub struct PromptParts {
    /// Role description, workflow steps, and output format — same for all tasks of this type.
    pub static_instructions: String,
    /// Project conventions, git config, sibling task warnings, rules — semi-static per session.
    pub context: String,
    /// Issue body, labels, comments, PR diff — changes every invocation.
    pub dynamic_payload: String,
}

impl PromptParts {
    /// Concatenate all three layers into the final prompt string.
    ///
    /// The output is identical to the `String` that was previously returned directly by
    /// the prompt-building function. Callers that previously stored the return value as a
    /// `String` should call this method to obtain it.
    pub fn to_prompt_string(&self) -> String {
        format!(
            "{}{}{}",
            self.static_instructions, self.context, self.dynamic_payload
        )
    }
}

/// Wrap `s` in POSIX single quotes, escaping any embedded single quotes via `'\''`.
///
/// This ensures the value is treated as literal data by the shell and cannot
/// break out of the quoting context or inject shell metacharacters.
pub(super) fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub(super) fn last_non_empty_line(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
}
