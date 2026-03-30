/// Defense-in-depth post-execution check: scan agent output for tool calls
/// that were not in the allowed list.
///
/// **Primary enforcement** is now at the CLI boundary via `--allowedTools`
/// (see `claude.rs`). This function is a secondary layer that catches any
/// tool invocations that appear in the output stream even when the CLI flag
/// is active — e.g. partial output logged before enforcement, or adapters
/// that do not support `--allowedTools`.
///
/// Returns the names of any disallowed tools found in the output. An empty
/// vector means no violation was detected in the output.
///
/// Detection heuristic: look for `<tool_name>` XML tags or JSON `"name":
/// "ToolName"` patterns that Claude Code typically emits in its output stream.
/// This is intentionally conservative — false positives are better than
/// silently missing a violation.
pub fn validate_tool_usage(output: &str, allowed_tools: &[String]) -> Vec<String> {
    if allowed_tools.is_empty() {
        return Vec::new();
    }

    let mut violations = Vec::new();

    // Known tools that harness is aware of. Only tools in this set are subject
    // to violation checking; unknown strings are ignored to avoid false positives
    // from user content that happens to look like a tool name.
    const KNOWN_TOOLS: &[&str] = &[
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "WebFetch",
        "WebSearch",
        "Task",
        "NotebookEdit",
        "TodoWrite",
        "TodoRead",
    ];

    let allowed_lower: Vec<String> = allowed_tools.iter().map(|t| t.to_lowercase()).collect();

    for tool in KNOWN_TOOLS {
        let tool_lower = tool.to_lowercase();
        if allowed_lower.contains(&tool_lower) {
            continue;
        }

        // Match patterns emitted by Claude Code:
        //   <tool_name>   (opening XML tag, case-insensitive)
        //   "name": "ToolName"   (JSON tool_call field)
        let xml_tag = format!("<{tool}");
        let json_name = format!("\"name\": \"{tool}\"");
        let json_name_nospace = format!("\"name\":\"{tool}\"");

        if output.contains(&xml_tag)
            || output.contains(&json_name)
            || output.contains(&json_name_nospace)
        {
            violations.push(tool.to_string());
        }
    }

    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agents::CapabilityProfile;

    // --- CapabilityProfile resolution tests ---

    #[test]
    fn read_only_profile_tools() {
        let tools = CapabilityProfile::ReadOnly.tools().unwrap();
        assert_eq!(tools, vec!["Read", "Grep", "Glob"]);
    }

    #[test]
    fn standard_profile_tools() {
        let tools = CapabilityProfile::Standard.tools().unwrap();
        assert_eq!(tools, vec!["Read", "Write", "Edit", "Bash"]);
    }

    #[test]
    fn full_profile_returns_none() {
        assert!(CapabilityProfile::Full.tools().is_none());
    }

    #[test]
    fn default_profile_is_full() {
        assert_eq!(CapabilityProfile::default(), CapabilityProfile::Full);
    }

    #[test]
    fn read_only_has_prompt_note() {
        assert!(CapabilityProfile::ReadOnly.prompt_note().is_some());
    }

    #[test]
    fn standard_has_prompt_note() {
        // Standard profile must provide a note so task_executor can inject it
        // into the prompt as secondary context alongside --allowedTools CLI enforcement.
        assert!(
            CapabilityProfile::Standard
                .prompt_note()
                .is_some_and(|n: &str| n.contains("Read")
                    && n.contains("Write")
                    && n.contains("Edit")
                    && n.contains("Bash")),
            "Standard prompt_note must mention all permitted tools"
        );
    }

    #[test]
    fn full_profile_no_prompt_note() {
        assert!(CapabilityProfile::Full.prompt_note().is_none());
    }

    // --- AgentsConfig resolve_allowed_tools tests ---

    #[test]
    fn explicit_allowed_tools_overrides_profile() {
        use crate::config::agents::AgentsConfig;
        let cfg = AgentsConfig {
            capability_profile: CapabilityProfile::ReadOnly,
            allowed_tools: Some(vec!["Bash".to_string()]),
            ..Default::default()
        };

        let resolved = cfg.resolve_allowed_tools().unwrap();
        assert_eq!(resolved, vec!["Bash"]);
    }

    #[test]
    fn profile_used_when_no_explicit_tools() {
        use crate::config::agents::AgentsConfig;
        let cfg = AgentsConfig {
            capability_profile: CapabilityProfile::Standard,
            ..Default::default()
        };

        let resolved = cfg.resolve_allowed_tools().unwrap();
        assert_eq!(resolved, vec!["Read", "Write", "Edit", "Bash"]);
    }

    #[test]
    fn full_profile_no_explicit_tools_returns_none() {
        use crate::config::agents::AgentsConfig;
        let cfg = AgentsConfig::default(); // Full profile, no allowed_tools
        assert!(cfg.resolve_allowed_tools().is_none());
    }

    // --- validate_tool_usage tests ---

    #[test]
    fn no_violations_when_allowed_list_empty() {
        let output = "<Write>foo</Write>";
        let violations = validate_tool_usage(output, &[]);
        assert!(violations.is_empty());
    }

    #[test]
    fn detects_disallowed_xml_tool_tag() {
        let output = "I will now call <Write>some content</Write>";
        let allowed = vec!["Read".to_string(), "Grep".to_string()];
        let violations = validate_tool_usage(output, &allowed);
        assert!(violations.contains(&"Write".to_string()));
    }

    #[test]
    fn detects_disallowed_json_tool_name() {
        let output = r#"{"name": "Bash", "input": {"cmd": "rm -rf /"}}"#;
        let allowed = vec!["Read".to_string()];
        let violations = validate_tool_usage(output, &allowed);
        assert!(violations.contains(&"Bash".to_string()));
    }

    #[test]
    fn no_violation_for_allowed_tool() {
        let output = "<Read>some/file.txt</Read>";
        let allowed = vec!["Read".to_string(), "Grep".to_string(), "Glob".to_string()];
        let violations = validate_tool_usage(output, &allowed);
        assert!(violations.is_empty());
    }

    #[test]
    fn multiple_violations_detected() {
        let output = "<Write>x</Write> and <Bash>y</Bash>";
        let allowed = vec!["Read".to_string()];
        let violations = validate_tool_usage(output, &allowed);
        assert!(violations.contains(&"Write".to_string()));
        assert!(violations.contains(&"Bash".to_string()));
    }
}
