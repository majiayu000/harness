//! Utility helper functions used across prompt builders.

use super::SiblingTask;

/// Wrap user-supplied content in delimiters to separate it from trusted instructions.
/// Escapes the closing tag within content to prevent delimiter injection.
pub fn wrap_external_data(content: &str) -> String {
    let escaped = content.replace("</external_data>", "<\\/external_data>");
    format!("<external_data>\n{}\n</external_data>", escaped)
}

/// Build a git instruction line from optional GitConfig.
/// Returns an empty string when `git` is None, or a sentence ending with "\n" when Some.
pub(super) fn git_config_line(git: Option<&crate::config::project::GitConfig>) -> String {
    match git {
        None => String::new(),
        Some(g) => format!(
            " Create your PR targeting the {} branch on the {} remote. \
             Use branch prefix {}.\n",
            g.base_branch, g.remote, g.branch_prefix
        ),
    }
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

/// Wrap `s` in POSIX single quotes, escaping any embedded single quotes via `'\''`.
///
/// This ensures the value is treated as literal data by the shell and cannot
/// break out of the quoting context or inject shell metacharacters.
pub(super) fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Sanitize a delimiter to prevent injection through user-supplied content.
pub fn sanitize_delimiter(s: &str) -> String {
    s.replace('<', "&lt;").replace('>', "&gt;")
}
