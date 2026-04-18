use super::issue::wrap_external_data;

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

/// Build prompt for the sprint planner agent.
///
/// The agent outputs a dependency graph (DAG), not rounds. The scheduler
/// handles parallelism by filling slots with ready tasks.
pub fn sprint_plan_prompt(issues: &str) -> String {
    let safe_issues = wrap_external_data(issues);
    format!(
        "You are an Engineering Manager planning a sprint.\n\n\
         ## Pending Issues\n\n\
         {safe_issues}\n\n\
         ## Task\n\n\
         For each issue, read its description and check the codebase to understand which \
         files it would touch. Then output a dependency graph:\n\
         - `depends_on`: list issue numbers that MUST complete before this one starts \
         (because they touch the same files or this fix builds on that fix)\n\
         - Empty `depends_on` means the issue can start immediately\n\
         - Higher priority (P0 > P1) issues should have fewer dependencies\n\
         - Mark issues as \"skip\" if already fixed, duplicate, or invalid\n\n\
         SPRINT_PLAN_START\n\
         {{\n\
           \"tasks\": [\n\
             {{\"issue\": 510, \"depends_on\": []}},\n\
             {{\"issue\": 514, \"depends_on\": []}},\n\
             {{\"issue\": 511, \"depends_on\": [510]}},\n\
             {{\"issue\": 515, \"depends_on\": [514]}}\n\
           ],\n\
           \"skip\": [\n\
             {{\"issue\": 452, \"reason\": \"fixed by recent commit\"}}\n\
           ]\n\
         }}\n\
         SPRINT_PLAN_END\n\n\
         Every pending issue must appear in tasks or skip.\n\
         The JSON between markers must be valid and parseable."
    )
}

/// Parse a `SprintPlan` from agent output by extracting JSON between markers.
/// Robust: finds the first `{` and last `}` between markers, ignoring
/// markdown fences, prose, or other wrapper text agents may add.
pub fn parse_sprint_plan(output: &str) -> Option<SprintPlan> {
    let start = output.find("SPRINT_PLAN_START")? + "SPRINT_PLAN_START".len();
    let end = output[start..].find("SPRINT_PLAN_END")? + start;
    let region = &output[start..end];
    // Extract the JSON object: first '{' to last '}'.
    let json_start = region.find('{')?;
    let json_end = region.rfind('}')? + 1;
    serde_json::from_str(&region[json_start..json_end]).ok()
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
