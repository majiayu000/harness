//! Prompt templates and output parsers shared across CLI and HTTP entries.

pub mod context;
pub mod lifecycle;
pub mod parsers;
pub mod review;
pub mod sprint;

pub use context::*;
pub use lifecycle::*;
pub use parsers::*;
pub use review::*;
pub use sprint::*;

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

/// Wrap user-supplied content in delimiters to separate it from trusted instructions.
/// Escapes the closing tag within content to prevent delimiter injection.
pub fn wrap_external_data(content: &str) -> String {
    let escaped = content.replace("</external_data>", "<\\/external_data>");
    format!("<external_data>\n{}\n</external_data>", escaped)
}

/// Returns the last non-empty (after trimming) line of `output`, trimmed.
pub(crate) fn last_non_empty_line(output: &str) -> Option<&str> {
    output
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
}
