//! Prompt templates and output parsers shared across CLI and HTTP entries.

pub mod helpers;
pub mod implementation;
pub mod parsers;
pub mod review;
pub mod sprint;
pub mod triage;

pub use helpers::{
    build_available_skills_listing, build_matched_skills_section, sanitize_delimiter,
    sibling_task_context, wrap_external_data,
};
pub use implementation::{
    check_existing_pr, continue_existing_pr, gc_adopt_prompt, implement_from_issue,
    implement_from_prompt,
};
pub use parsers::{
    extract_pr_number, extract_review_issues, is_approved, is_lgtm, is_waiting,
    parse_github_pr_url, parse_issue_count, parse_pr_url, repo_slug_from_pr_url,
};
pub use review::{
    agent_review_fix_prompt, agent_review_intervention_prompt, agent_review_prompt,
    periodic_review_prompt, periodic_review_prompt_with_guard_scan, review_prompt,
    review_synthesis_prompt, review_synthesis_prompt_with_agents,
};
pub use sprint::{evaluator_prompt, parse_sprint_plan, sprint_contract_prompt, sprint_plan_prompt};
pub use triage::{parse_complexity, parse_plan_issue, parse_triage, plan_prompt, triage_prompt};

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

/// Task complexity assessed during the triage phase.
///
/// Used to derive dynamic `max_rounds` and `skip_agent_review` parameters so that
/// simple tasks don't waste review budget and complex tasks get extra rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageComplexity {
    /// Typo, doc fix, or single-file config change — max_rounds=2, skip agent review.
    Low,
    /// Single-module change — max_rounds=4 (current default), full review.
    Medium,
    /// Cross-file refactor or new API surface — max_rounds=8, full review.
    High,
}

/// Triage decision from the Tech Lead evaluation phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageDecision {
    /// Trivial change — skip planning, go straight to implement.
    Proceed,
    /// Complex change — needs an implementation plan first.
    ProceedWithPlan,
    /// Issue is ambiguous — questions must be answered before proceeding.
    NeedsClarification,
    /// Not worth doing — skip this issue.
    Skip,
}

/// Describes a sibling task running in parallel on the same project.
///
/// Used by [`sibling_task_context`] to build a constraint block for the agent.
pub struct SiblingTask {
    /// GitHub issue number, if this is an issue-based task.
    pub issue: Option<u64>,
    /// Short description of what the sibling task is implementing.
    pub description: String,
}

/// A task node in the sprint DAG.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct SprintTask {
    pub issue: u64,
    #[serde(default)]
    pub depends_on: Vec<u64>,
}

/// An issue the planner decided to skip.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct SprintSkip {
    pub issue: u64,
    pub reason: String,
}

/// DAG-based sprint plan. The scheduler fills slots with tasks whose
/// dependencies are satisfied, keeping all slots busy at all times.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct SprintPlan {
    pub tasks: Vec<SprintTask>,
    #[serde(default)]
    pub skip: Vec<SprintSkip>,
}

#[cfg(test)]
mod tests;
