//! Triage and plan phase prompts and parsers.

use super::helpers::{git_config_line, wrap_external_data};
use super::parsers::last_non_empty_line;
use super::{PromptParts, TriageComplexity, TriageDecision};

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

/// Parse the triage decision from agent output.
///
/// Looks for `TRIAGE=<decision>` on the last non-empty line.
/// Returns `None` if the output doesn't contain a valid triage line.
pub fn parse_triage(output: &str) -> Option<TriageDecision> {
    let last = last_non_empty_line(output)?;
    let value = last.strip_prefix("TRIAGE=")?;
    match value.trim() {
        "PROCEED" => Some(TriageDecision::Proceed),
        "PROCEED_WITH_PLAN" => Some(TriageDecision::ProceedWithPlan),
        "NEEDS_CLARIFICATION" => Some(TriageDecision::NeedsClarification),
        "SKIP" => Some(TriageDecision::Skip),
        _ => None,
    }
}

/// Parse `COMPLEXITY=<low|medium|high>` from triage agent output.
///
/// The protocol requires `COMPLEXITY=` to appear on the second-to-last line
/// (just before `TRIAGE=` on the last line). Only the second-to-last non-empty
/// line is examined to prevent untrusted issue content (e.g. a line containing
/// `COMPLEXITY=low` in the issue body) from influencing classification.
///
/// Returns `TriageComplexity::Medium` if the tag is absent or unrecognised
/// (backward-compatible fallback).
pub fn parse_complexity(output: &str) -> TriageComplexity {
    let non_empty: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
    // second-to-last line (index len-2); need at least 2 non-empty lines
    if non_empty.len() < 2 {
        return TriageComplexity::Medium;
    }
    let candidate = non_empty[non_empty.len() - 2].trim();
    if let Some(value) = candidate.strip_prefix("COMPLEXITY=") {
        return match value.trim().to_ascii_lowercase().as_str() {
            "low" => TriageComplexity::Low,
            "high" => TriageComplexity::High,
            _ => TriageComplexity::Medium,
        };
    }
    TriageComplexity::Medium
}

/// Check if the agent flagged an issue with the implementation plan.
///
/// During the implement phase, if the plan is wrong the agent outputs
/// `PLAN_ISSUE=<description>` instead of implementing. Returns the
/// description if present.
pub fn parse_plan_issue(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let line = line.trim();
        if let Some(desc) = line.strip_prefix("PLAN_ISSUE=") {
            return Some(desc.trim().to_string());
        }
    }
    None
}

// Suppress unused import warning — git_config_line is re-exported via helpers
// but referenced here to satisfy the import.
#[allow(unused_imports)]
use self::git_config_line as _;
