//! Sibling-task context and skill listing helpers.

use super::wrap_external_data;

/// Describes a sibling task running in parallel on the same project.
///
/// Used by [`sibling_task_context`] to build a constraint block for the agent.
pub struct SiblingTask {
    /// GitHub issue number, if this is an issue-based task.
    pub issue: Option<u64>,
    /// Short description of what the sibling task is implementing.
    pub description: String,
}

/// Build a warning block telling the agent which issues other parallel agents are handling.
///
/// When `siblings` is empty, returns an empty string so callers can skip appending it.
/// The block instructs the agent to stay in its lane and avoid modifying files owned by
/// siblings, which reduces cross-agent merge conflicts in parallel dispatch scenarios.
///
/// Sibling descriptions are user-supplied text and are wrapped with [`wrap_external_data`]
/// so the agent treats them as untrusted data rather than trusted instructions.
pub fn sibling_task_context(siblings: &[SiblingTask]) -> String {
    if siblings.is_empty() {
        return String::new();
    }
    let mut desc_lines: Vec<String> = Vec::with_capacity(siblings.len());
    for s in siblings {
        match s.issue {
            Some(n) => desc_lines.push(format!("- #{n}: {}", s.description)),
            None => desc_lines.push(format!("- {}", s.description)),
        }
    }
    let safe_list = wrap_external_data(&desc_lines.join("\n"));
    format!(
        "\u{26a0}\u{fe0f}  The following issues are being handled by OTHER agents in parallel on this same project.\n\
         Do NOT modify files related to these issues — another agent is responsible:\n\
         {safe_list}\n\
         \nOnly modify files directly needed for YOUR assigned task."
    )
}

/// Build the "Available Skills" listing injected into every agent prompt.
///
/// Each entry is a `(name, description)` pair. Returns an empty string when
/// the iterator is empty so callers can skip appending.
pub fn build_available_skills_listing<'a>(
    skills: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut iter = skills.into_iter().peekable();
    if iter.peek().is_none() {
        return String::new();
    }
    let mut out = "\n\n## Available Skills\n".to_string();
    for (name, desc) in iter {
        out.push_str("- **");
        out.push_str(name);
        out.push_str("**: ");
        out.push_str(desc);
        out.push('\n');
    }
    out
}

/// Build the "Relevant Skills" section for skills whose trigger patterns matched
/// the current prompt.
///
/// Each entry is a `(name, content)` pair. Returns an empty string when the
/// iterator is empty so callers can skip appending.
pub fn build_matched_skills_section<'a>(
    skills: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    let mut iter = skills.into_iter().peekable();
    if iter.peek().is_none() {
        return String::new();
    }
    let mut out = "\n\n## Relevant Skills\n".to_string();
    for (name, content) in iter {
        out.push_str("### ");
        out.push_str(name);
        out.push('\n');
        out.push_str(content);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sibling_task_context_empty() {
        assert_eq!(sibling_task_context(&[]), String::new());
    }

    #[test]
    fn test_sibling_task_context_with_issue_siblings() {
        let siblings = vec![
            SiblingTask {
                issue: Some(77),
                description: "fix Mistral transform_request unwrap".to_string(),
            },
            SiblingTask {
                issue: Some(78),
                description: "fix Vertex AI unwrap".to_string(),
            },
        ];
        let ctx = sibling_task_context(&siblings);
        assert!(ctx.contains("#77: fix Mistral transform_request unwrap"));
        assert!(ctx.contains("#78: fix Vertex AI unwrap"));
        assert!(ctx.contains("OTHER agents"));
        assert!(ctx.contains("Only modify files directly needed for YOUR assigned task."));
    }

    #[test]
    fn test_sibling_task_context_without_issue_number() {
        let siblings = vec![SiblingTask {
            issue: None,
            description: "refactor auth middleware".to_string(),
        }];
        let ctx = sibling_task_context(&siblings);
        assert!(ctx.contains("- refactor auth middleware"));
        assert!(!ctx.contains('#'));
    }

    #[test]
    fn build_available_skills_listing_empty_returns_empty_string() {
        assert!(build_available_skills_listing(std::iter::empty::<(&str, &str)>()).is_empty());
    }

    #[test]
    fn build_available_skills_listing_formats_all_entries() {
        let skills = [
            ("review", "Code review tool"),
            ("deploy", "Deploy to production"),
        ];
        let result = build_available_skills_listing(skills.iter().copied());
        assert!(result.contains("## Available Skills"));
        assert!(result.contains("**review**"));
        assert!(result.contains("Code review tool"));
        assert!(result.contains("**deploy**"));
        assert!(result.contains("Deploy to production"));
    }

    #[test]
    fn build_available_skills_listing_starts_with_double_newline() {
        let skills = [("a", "desc")];
        let result = build_available_skills_listing(skills.iter().copied());
        assert!(
            result.starts_with("\n\n"),
            "section must start with two newlines"
        );
    }

    #[test]
    fn build_matched_skills_section_empty_returns_empty_string() {
        assert!(build_matched_skills_section(std::iter::empty::<(&str, &str)>()).is_empty());
    }

    #[test]
    fn build_matched_skills_section_formats_name_and_content() {
        let skills = [("review", "# Review\nReview code carefully.")];
        let result = build_matched_skills_section(skills.iter().copied());
        assert!(result.contains("## Relevant Skills"));
        assert!(result.contains("### review"));
        assert!(result.contains("Review code carefully."));
    }

    #[test]
    fn build_matched_skills_section_starts_with_double_newline() {
        let skills = [("a", "content")];
        let result = build_matched_skills_section(skills.iter().copied());
        assert!(
            result.starts_with("\n\n"),
            "section must start with two newlines"
        );
    }
}
