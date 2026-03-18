use harness_core::prompts;
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct GhPrListItem {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HarnessMentionCommand {
    Mention,
    Review,
    FixCi,
}

/// Parse the first `@harness` mention found while scanning line-by-line.
/// For each line, only the first `@harness` occurrence is considered.
pub(crate) fn parse_harness_mention_command(body: &str) -> Option<HarnessMentionCommand> {
    for line in body.lines() {
        let lowercase = line.trim().to_ascii_lowercase();
        if let Some(idx) = lowercase.find("@harness") {
            let mut command = lowercase[idx + "@harness".len()..].trim_start();
            command = command.trim_start_matches(|ch: char| {
                ch.is_whitespace() || ch == ':' || ch == ',' || ch == '-' || ch == '.'
            });

            if command.starts_with("fix ci")
                || command.starts_with("fix-ci")
                || command.starts_with("fix_ci")
            {
                return Some(HarnessMentionCommand::FixCi);
            }
            if command.starts_with("review") {
                return Some(HarnessMentionCommand::Review);
            }
            return Some(HarnessMentionCommand::Mention);
        }
    }

    None
}

/// Fluent builder for constructing structured agent prompts.
///
/// Produces a prompt with a title line followed by zero or more named
/// sections and optional URL metadata lines.  All external-data payloads are
/// automatically wrapped in `<external_data>` tags to reduce prompt-injection
/// risk.
pub(crate) struct PromptBuilder {
    title: String,
    sections: Vec<(String, String)>,
}

impl PromptBuilder {
    /// Create a new builder with the given title line.
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            sections: Vec::new(),
        }
    }

    /// Add a named section with `content` wrapped in external_data tags.
    pub(crate) fn add_section(mut self, name: &str, content: &str) -> Self {
        self.sections
            .push((name.to_string(), prompts::wrap_external_data(content)));
        self
    }

    /// Add an optional URL metadata line. No-op if `url` is `None`.
    pub(crate) fn add_optional_url(mut self, label: &str, url: Option<&str>) -> Self {
        if let Some(u) = url {
            let safe = prompts::wrap_external_data(u);
            self.sections
                .push((String::new(), format!("- {label}: {safe}")));
        }
        self
    }

    /// Assemble the prompt: title, then each section, with a trailing newline.
    pub(crate) fn build(self) -> String {
        let mut out = self.title;
        for (name, content) in &self.sections {
            out.push('\n');
            if name.is_empty() {
                out.push_str(content);
            } else {
                out.push_str(name);
                out.push_str(":\n");
                out.push_str(content);
            }
        }
        out.push('\n');
        out
    }
}

pub(crate) fn build_fix_ci_prompt(
    repository: &str,
    pr_number: u64,
    comment_body: &str,
    comment_url: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");
    let preamble = PromptBuilder::new(format!(
        "CI failure repair requested for PR #{pr_number} in `{repository}`."
    ))
    .add_optional_url("Trigger comment", comment_url)
    .add_optional_url("PR URL", pr_url)
    .add_section("Command payload", comment_body)
    .build();

    format!(
        "{preamble}\n\
         Required workflow:\n\
         1. Inspect failing checks for PR #{pr_number} (`gh pr checks {pr_number}`)\n\
         2. Investigate CI failure details from logs and failing tests\n\
         3. Implement a minimal fix that makes CI green\n\
         4. Run the repository's standard validation commands for the affected changes (including all failing/required CI checks)\n\
         5. Commit and push to the existing PR branch\n\n\
         On the last line, print PR_URL={canonical_pr_url}"
    )
}

pub(crate) fn build_pr_rework_prompt(
    repository: &str,
    pr_number: u64,
    review_state: &str,
    review_body: &str,
    review_url: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");
    let preamble = PromptBuilder::new(format!(
        "PR review feedback received on PR #{pr_number} in `{repository}`.\nReview state: {review_state}"
    ))
    .add_optional_url("Review URL", review_url)
    .add_optional_url("PR URL", pr_url)
    .add_section("Review feedback", review_body)
    .build();

    format!(
        "{preamble}\n\
         Required workflow:\n\
         1. Read the review feedback above carefully.\n\
         2. Address all requested changes.\n\
         3. Run the repository's standard validation commands.\n\
         4. Commit and push to the existing PR branch (do not create a new PR).\n\n\
         On the last line, print PR_URL={canonical_pr_url}"
    )
}

pub(crate) fn build_pr_approved_prompt(
    repository: &str,
    pr_number: u64,
    review_url: Option<&str>,
) -> String {
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");
    let preamble = PromptBuilder::new(format!(
        "PR #{pr_number} in `{repository}` has been approved by a reviewer."
    ))
    .add_optional_url("Review URL", review_url)
    .build();

    format!(
        "{preamble}\n\
         Action required:\n\
         Post a comment on the PR indicating it is ready to merge:\n\
         gh pr comment {pr_number} --repo {repository} --body \"Approved — ready to merge.\"\n\n\
         Then stop. There is nothing else to implement.\n\n\
         On the last line, print PR_URL={canonical_pr_url}"
    )
}

/// Query GitHub for an existing open PR linked to the given issue.
/// Returns `(pr_number, branch_name)` if found.
pub(crate) async fn find_existing_pr_for_issue(
    project: &Path,
    issue: u64,
) -> anyhow::Result<Option<(u64, String)>> {
    let output = Command::new("gh")
        .current_dir(project)
        .args([
            "pr",
            "list",
            "--search",
            &format!("#{issue}"),
            "--state",
            "open",
        ])
        .args(["--json", "number,headRefName", "--limit", "1"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to run `gh pr list` for issue #{issue}: {e}"))?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "`gh pr list` for issue #{issue} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let items: Vec<GhPrListItem> = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow::anyhow!("invalid JSON from `gh pr list`: {e}"))?;

    Ok(items
        .into_iter()
        .next()
        .map(|item| (item.number, item.head_ref_name)))
}

#[cfg(test)]
mod tests {
    use super::PromptBuilder;

    #[test]
    fn prompt_builder_no_sections_adds_trailing_newline() {
        let result = PromptBuilder::new("Title line.").build();
        assert_eq!(result, "Title line.\n");
    }

    #[test]
    fn prompt_builder_optional_url_absent_is_skipped() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("Link", None)
            .build();
        assert_eq!(result, "Title.\n");
    }

    #[test]
    fn prompt_builder_optional_url_present_appears_in_output() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("Link", Some("https://example.com"))
            .build();
        assert!(result.contains("- Link: "));
        assert!(result.contains("https://example.com"));
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn prompt_builder_add_section_wraps_external_data() {
        let result = PromptBuilder::new("Title.")
            .add_section("Payload", "content here")
            .build();
        assert!(result.contains("Payload:\n"));
        assert!(result.contains("<external_data>"));
        assert!(result.contains("content here"));
    }

    #[test]
    fn prompt_builder_multiple_urls_all_appear() {
        let result = PromptBuilder::new("Title.")
            .add_optional_url("First", Some("url1"))
            .add_optional_url("Second", None)
            .add_optional_url("Third", Some("url3"))
            .build();
        assert!(result.contains("- First: "));
        assert!(result.contains("url1"));
        assert!(!result.contains("Second"));
        assert!(result.contains("- Third: "));
        assert!(result.contains("url3"));
    }
}
