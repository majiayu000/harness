//! GitHub PR and repository detection shared by intake and workflow runtime.

use harness_core::prompts;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosingPullRequestCandidate {
    pub(crate) repo_slug: String,
    pub(crate) number: u64,
    pub(crate) head_ref_name: String,
    pub(crate) url: String,
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

pub(crate) struct PromptBuilder {
    title: String,
    sections: Vec<(String, String)>,
}

impl PromptBuilder {
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

/// Parse `"owner/repo"` from a git remote URL.
///
/// Handles HTTPS (`https://github.com/owner/repo.git`),
/// SCP-style SSH (`git@github.com:owner/repo.git`), and
/// ssh-scheme SSH (`ssh://git@github.com/owner/repo.git`) formats.
pub(crate) fn parse_repo_slug_from_remote_url(url: &str) -> Option<String> {
    // SCP-style SSH: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let slug = rest.trim_end_matches(".git");
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }
    // ssh-scheme SSH: ssh://git@github.com/owner/repo.git
    if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        let slug = rest.trim_end_matches(".git");
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }
    // HTTPS: https://github.com/owner/repo.git
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let slug = rest.trim_end_matches(".git");
        if slug.contains('/') {
            return Some(slug.to_string());
        }
    }
    None
}

/// Detect the `"owner/repo"` slug by reading configured git remotes from
/// `.git/config`.
///
/// This intentionally avoids launching `git`. It prefers `origin` for
/// stability but falls back to any other GitHub remote, which keeps the
/// cross-repo guard active in repositories whose primary remote has a
/// different name.
pub(crate) async fn detect_repo_slug(project: &Path) -> Option<String> {
    let mut remotes = Vec::new();
    for config_path in git_config_candidates(project) {
        let Ok(config) = std::fs::read_to_string(&config_path) else {
            continue;
        };
        remotes.extend(parse_remote_urls_from_git_config(&config));
    }

    let mut fallback: Option<String> = None;
    for (name, url) in remotes {
        if let Some(slug) = parse_repo_slug_from_remote_url(&url) {
            if name == "origin" {
                return Some(slug);
            }
            if fallback.is_none() {
                fallback = Some(slug);
            }
        }
    }
    fallback
}

fn git_config_candidates(project: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut current = if project.is_dir() {
        Some(project)
    } else {
        project.parent()
    };
    while let Some(dir) = current {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            candidates.push(dotgit.join("config"));
            break;
        }
        if dotgit.is_file() {
            candidates.extend(config_candidates_from_gitdir_file(&dotgit));
            break;
        }
        current = dir.parent();
    }
    candidates
}

fn config_candidates_from_gitdir_file(dotgit: &Path) -> Vec<PathBuf> {
    let Ok(contents) = std::fs::read_to_string(dotgit) else {
        return Vec::new();
    };
    let Some(raw_gitdir) = contents.trim().strip_prefix("gitdir:") else {
        return Vec::new();
    };
    let gitdir = {
        let path = PathBuf::from(raw_gitdir.trim());
        if path.is_absolute() {
            path
        } else {
            dotgit
                .parent()
                .map(|parent| parent.join(&path))
                .unwrap_or(path)
        }
    };

    let mut candidates = vec![gitdir.join("config")];
    let commondir = gitdir.join("commondir");
    if let Ok(raw_common) = std::fs::read_to_string(&commondir) {
        let common_path = PathBuf::from(raw_common.trim());
        let common_path = if common_path.is_absolute() {
            common_path
        } else {
            gitdir.join(common_path)
        };
        candidates.push(common_path.join("config"));
    }
    if let Some(common_git_dir) = gitdir.parent().and_then(|p| p.parent()) {
        candidates.push(common_git_dir.join("config"));
    }
    candidates
}

fn parse_remote_urls_from_git_config(config: &str) -> Vec<(String, String)> {
    let mut current_remote: Option<String> = None;
    let mut remotes = Vec::new();
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = &trimmed[1..trimmed.len() - 1];
            current_remote = parse_remote_section_name(section);
            continue;
        }
        let Some(name) = current_remote.as_deref() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            let url = trim_git_config_value(value);
            if !url.is_empty() {
                remotes.push((name.to_string(), url.to_string()));
            }
        }
    }
    remotes
}

fn parse_remote_section_name(section: &str) -> Option<String> {
    let section = section.trim();
    if let Some(name) = section.strip_prefix("remote.") {
        let name = name.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }

    let rest = section.strip_prefix("remote")?;
    if rest.chars().next().is_some_and(|ch| !ch.is_whitespace()) {
        return None;
    }
    let rest = rest.trim_start();
    let quoted = rest.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let name = quoted[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn trim_git_config_value(value: &str) -> &str {
    let value = value.trim();
    for (idx, ch) in value.char_indices() {
        if (ch == '#' || ch == ';')
            && value[..idx]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            return value[..idx].trim_end();
        }
    }
    value
}

#[cfg(test)]
#[path = "pr_detection_tests.rs"]
mod tests;
