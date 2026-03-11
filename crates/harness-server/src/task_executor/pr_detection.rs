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

pub(crate) fn build_fix_ci_prompt(
    repository: &str,
    pr_number: u64,
    comment_body: &str,
    comment_url: Option<&str>,
    pr_url: Option<&str>,
) -> String {
    let wrapped_comment = prompts::wrap_external_data(comment_body);
    let safe_comment_url = comment_url.map(prompts::wrap_external_data);
    let comment_url_line = safe_comment_url
        .as_deref()
        .map(|url| format!("- Trigger comment: {url}\n"))
        .unwrap_or_default();
    let safe_pr_url = pr_url.map(prompts::wrap_external_data);
    let pr_url_line = safe_pr_url
        .as_deref()
        .map(|url| format!("- PR URL: {url}\n"))
        .unwrap_or_default();
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");

    format!(
        "CI failure repair requested for PR #{pr_number} in `{repository}`.\n\
         {comment_url_line}\
         {pr_url_line}\
         Command payload:\n\
         {wrapped_comment}\n\n\
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
    let wrapped_body = prompts::wrap_external_data(review_body);
    let safe_review_url = review_url.map(prompts::wrap_external_data);
    let review_url_line = safe_review_url
        .as_deref()
        .map(|url| format!("- Review URL: {url}\n"))
        .unwrap_or_default();
    let safe_pr_url = pr_url.map(prompts::wrap_external_data);
    let pr_url_line = safe_pr_url
        .as_deref()
        .map(|url| format!("- PR URL: {url}\n"))
        .unwrap_or_default();
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");

    format!(
        "PR review feedback received on PR #{pr_number} in `{repository}`.\n\
         Review state: {review_state}\n\
         {review_url_line}\
         {pr_url_line}\
         Review feedback:\n\
         {wrapped_body}\n\n\
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
    let safe_review_url = review_url.map(prompts::wrap_external_data);
    let review_url_line = safe_review_url
        .as_deref()
        .map(|url| format!("- Review URL: {url}\n"))
        .unwrap_or_default();
    let canonical_pr_url = format!("https://github.com/{repository}/pull/{pr_number}");

    format!(
        "PR #{pr_number} in `{repository}` has been approved by a reviewer.\n\
         {review_url_line}\n\
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
