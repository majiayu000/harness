//! Prompts for the cross-review (primary + challenger) workflow.

/// Prompt sent to the primary reviewer agent.
///
/// `safe_target` must already be wrapped via [`super::wrap_external_data`] so
/// that user-controlled review content cannot escape the prompt boundary.
pub fn primary_review_prompt(safe_target: &str) -> String {
    format!(
        "Review the following target for issues:\n{safe_target}\n\n\
         List each issue on its own line prefixed with \"ISSUE: \".\n\
         If no issues are found, output: LGTM"
    )
}

/// Prompt sent to the challenger reviewer agent.
///
/// `safe_primary` must already be wrapped via [`super::wrap_external_data`].
/// `outstanding` is appended verbatim and may be empty; when non-empty it
/// should already start with the leading delimiter the caller wants
/// (typically `"\n\nOutstanding issues from previous round:\n- …"`).
pub fn challenger_prompt(safe_primary: &str, outstanding: &str) -> String {
    format!(
        "You are a challenger reviewer. Given this primary code review:\n{safe_primary}\n\n\
         For each ISSUE listed, classify it on its own line:\n\
         CONFIRMED: <issue>  — the issue is a real problem\n\
         FALSE-POSITIVE: <issue>  — the issue is wrong or not applicable\n\
         Also list any issues the primary review missed as:\n\
         MISSED: <new issue>{outstanding}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_prompt_contains_target_and_lgtm_marker() {
        let prompt = primary_review_prompt("WRAPPED_TARGET");
        assert!(prompt.contains("WRAPPED_TARGET"));
        assert!(prompt.contains("ISSUE: "));
        assert!(prompt.contains("LGTM"));
    }

    #[test]
    fn challenger_prompt_handles_empty_outstanding() {
        let prompt = challenger_prompt("WRAPPED_PRIMARY", "");
        assert!(prompt.contains("WRAPPED_PRIMARY"));
        assert!(prompt.contains("CONFIRMED:"));
        assert!(prompt.contains("FALSE-POSITIVE:"));
        assert!(prompt.contains("MISSED:"));
        assert!(prompt.ends_with("MISSED: <new issue>"));
    }

    #[test]
    fn challenger_prompt_appends_outstanding() {
        let prompt = challenger_prompt("X", "\n\nOutstanding issues from previous round:\n- foo");
        assert!(prompt.contains("Outstanding issues from previous round:\n- foo"));
    }
}
