//! Prompts for the GC `learn` flow that mines reusable rules and skills out of
//! adopted remediation drafts.

use super::wrap_external_data;

/// Prompt that asks the agent to extract reusable guard rules from a list of
/// adopted draft contents.
pub fn rules_extraction_prompt(draft_contents: &[String]) -> String {
    let joined = draft_contents.join("\n\n---\n\n");
    let safe_content = wrap_external_data(&joined);
    format!(
        "Analyze the following adopted remediation drafts and extract reusable guard rules.\n\
         {safe_content}\n\n\
         For each distinct rule pattern you identify, output a block in this exact format:\n\
         ## RULE_ID: Short title\n\
         severity: high\n\
         Description of the rule and why it matters.\n\n\
         Use severity values: critical, high, medium, or low.\n\
         Use RULE_IDs like LEARN-001, LEARN-002, etc.\n\
         Output only rule blocks, no other text."
    )
}

/// Prompt that asks the agent to extract reusable skills from a list of
/// adopted draft contents.
pub fn skills_extraction_prompt(draft_contents: &[String]) -> String {
    let joined = draft_contents.join("\n\n---\n\n");
    let safe_content = wrap_external_data(&joined);
    format!(
        "Analyze the following adopted remediation drafts and extract reusable skills.\n\
         {safe_content}\n\n\
         For each reusable skill or pattern you identify, output a block in this exact format:\n\
         === skill: kebab-case-name ===\n\
         # Skill Title\n\
         Description and usage instructions.\n\n\
         Output only skill blocks, no other text."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_prompt_includes_drafts_and_format_marker() {
        let drafts = vec!["draft A".to_string(), "draft B".to_string()];
        let prompt = rules_extraction_prompt(&drafts);
        assert!(prompt.contains("draft A"));
        assert!(prompt.contains("draft B"));
        assert!(prompt.contains("---"));
        assert!(prompt.contains("## RULE_ID:"));
        assert!(prompt.contains("severity: high"));
    }

    #[test]
    fn skills_prompt_includes_drafts_and_format_marker() {
        let drafts = vec!["s1".to_string()];
        let prompt = skills_extraction_prompt(&drafts);
        assert!(prompt.contains("s1"));
        assert!(prompt.contains("=== skill: kebab-case-name ==="));
    }

    #[test]
    fn handles_empty_drafts() {
        let prompt = rules_extraction_prompt(&[]);
        // Empty join produces a wrapped empty payload; the structure of the
        // surrounding instructions must still be intact.
        assert!(prompt.contains("Analyze the following adopted remediation drafts"));
        assert!(prompt.contains("Output only rule blocks"));
    }
}
