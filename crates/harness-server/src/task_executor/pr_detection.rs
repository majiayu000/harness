use harness_core::prompts;
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
struct GhPrListItem {
    number: u64,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    /// Full PR URL, e.g. `https://github.com/owner/repo/pull/42`.
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
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

/// Query GitHub for an existing open PR that *claims to close* the given issue.
/// Returns `(pr_number, branch_name, pr_url)` if found.
///
/// `gh pr list --search "#N"` is a free-text search: it returns any PR whose
/// title, body, or comments merely mention `#N`. That led to cross-issue
/// pollution when one PR's body referenced another issue number as context
/// (e.g. PR for #794 that says "depends on #791 being fixed first") and a
/// later task for #791 then tried to "continue" on that PR's branch.
///
/// This function now filters results so only PRs that **explicitly declare a
/// closing relationship** to the issue are returned. The match is any of:
/// - a closing keyword in title or body: `closes|closed|close|fixes|fixed|fix|resolves|resolved|resolve #N`
/// - the `(#N)` suffix pattern that harness uses in its own PR titles
pub(crate) async fn find_existing_pr_for_issue(
    project: &Path,
    issue: u64,
) -> anyhow::Result<Option<(u64, String, String)>> {
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
        // Fetch more fields so we can distinguish "this PR closes #N" from
        // "this PR merely mentions #N". The search is free-text (title, body,
        // and comments), so widely-referenced issues can accumulate many
        // mention-only results before the real closing PR appears. Use a limit
        // of 100 to ensure we scan enough candidates without missing the one
        // PR that actually claims to close the issue.
        .args([
            "--json",
            "number,headRefName,url,title,body",
            "--limit",
            "100",
        ])
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

    let repo_slug = detect_repo_slug(project).await;

    Ok(items
        .into_iter()
        .find(|item| pr_claims_to_close_issue(item, issue, repo_slug.as_deref()))
        .map(|item| (item.number, item.head_ref_name, item.url)))
}

/// Return true iff `item` declares a closing relationship to `issue` — i.e.
/// this PR, when merged, should close the issue.
///
/// A plain mention of `#N` in the body (e.g. "related to #N", "depends on #N")
/// is NOT sufficient and must return false, otherwise one issue's PR can be
/// silently reused for a different issue.
///
/// The `(#N)` trailing-title suffix is intentionally NOT treated as a close
/// signal here: PR creation prompts do not contractually enforce that suffix,
/// so any manually-created or unrelated PR whose title happens to end with
/// `(#N)` (e.g. `chore: cleanup scheduler (#799)`) would be falsely matched.
/// Only GitHub's closing-keyword syntax is reliable.
///
/// `repo_slug` is the `owner/repo` identifier for the current repository.
/// When provided, repo-qualified references (e.g. `Fixes owner/repo#N`) are
/// only accepted if the qualifier matches `repo_slug`, preventing cross-repo
/// false positives. When `None` the qualifier is accepted as-is.
fn pr_claims_to_close_issue(item: &GhPrListItem, issue: u64, repo_slug: Option<&str>) -> bool {
    let title = item.title.to_ascii_lowercase();
    let body = item.body.to_ascii_lowercase();
    // Scan title and body independently so that a keyword at the end of the
    // title cannot be paired with `#N` at the start of the body.
    field_claims_to_close_issue(&title, issue, repo_slug)
        || field_claims_to_close_issue(&body, issue, repo_slug)
}

fn field_claims_to_close_issue(field: &str, issue: u64, repo_slug: Option<&str>) -> bool {
    // GitHub's auto-close keywords (case-insensitive per GitHub docs).
    // See: https://docs.github.com/en/get-started/writing-on-github/working-with-advanced-formatting/using-keywords-in-issues-and-pull-requests
    // Word boundary on the left prevents e.g. "prefixes #5" from matching "fixes #5".
    const CLOSE_KEYWORDS: &[&str] = &[
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let needle = format!("#{issue}");
    // Scan each occurrence of `#N` and check:
    //   1. the preceding token is a close keyword, AND
    //   2. the matched `#N` is not itself a prefix of a longer issue reference
    //      (e.g. scanning for `#79` must not match `#791` or `#791abc`).
    for (idx, _) in field.match_indices(needle.as_str()) {
        let after = &field[idx + needle.len()..];
        if after.chars().next().is_some_and(|c| c.is_alphanumeric()) {
            continue;
        }
        let prefix = &field[..idx];
        // Skip trailing whitespace/punctuation between keyword and `#N`.
        let trimmed = prefix.trim_end_matches(|c: char| c.is_whitespace() || c == ':');
        if CLOSE_KEYWORDS
            .iter()
            .any(|kw| trimmed.ends_with(*kw) && word_boundary_before(trimmed, kw.len()))
        {
            return true;
        }
        // Also handle GitHub's repo-qualified syntax: "Fixes owner/repo#N".
        // Strip the trailing "owner/repo" slug and check the remainder for a keyword.
        if let Some(remainder) = strip_trailing_repo_qualifier(trimmed) {
            // Reject references to a different repository.
            if let Some(slug) = repo_slug {
                let qualifier = trimmed[remainder.len()..].trim();
                if !qualifier.eq_ignore_ascii_case(slug) {
                    continue;
                }
            }
            let inner = remainder.trim_end_matches(|c: char| c.is_whitespace() || c == ':');
            if CLOSE_KEYWORDS
                .iter()
                .any(|kw| inner.ends_with(*kw) && word_boundary_before(inner, kw.len()))
            {
                return true;
            }
        }
    }
    false
}

/// Check whether the character just before a trailing substring of length `kw_len`
/// is a word boundary (non-alphanumeric), so that "prefix-fix" does not match
/// the keyword "fix".
fn word_boundary_before(s: &str, kw_len: usize) -> bool {
    let start = s.len().saturating_sub(kw_len);
    if start == 0 {
        return true;
    }
    // Use char-aware look-back so that multi-byte UTF-8 sequences are handled
    // correctly. A raw-byte check would treat the trailing byte of a multi-byte
    // character (e.g. `é` → 0xC3 0xA9) as non-ASCII and therefore non-
    // alphanumeric, falsely reporting a word boundary for inputs like
    // `éfixes #791`.
    s[..start]
        .chars()
        .next_back()
        .map(|c| !c.is_alphanumeric())
        .unwrap_or(true)
}

/// If `s` ends with an `owner/repo` slug pattern, return the portion before the slug.
/// Used to detect repo-qualified close references like `fixes owner/repo#N`.
fn strip_trailing_repo_qualifier(s: &str) -> Option<&str> {
    let slash_pos = s.rfind('/')?;
    let repo_part = &s[slash_pos + 1..];
    if repo_part.is_empty()
        || !repo_part
            .chars()
            .all(|c: char| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }
    let before_slash = &s[..slash_pos];
    // Find the start of the owner segment: last non-slug character before the slash.
    let owner_start = before_slash
        .rfind(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
        .map(|i| i + 1)
        .unwrap_or(0);
    if owner_start >= slash_pos {
        return None;
    }
    Some(&s[..owner_start])
}

/// Attempt to recover a PR URL when the agent output lacked a `PR_URL=` sentinel.
///
/// Detects the current branch of `project_root` via `git rev-parse --abbrev-ref HEAD`,
/// then queries `gh pr list --search "head:<branch>"` for open PRs on that branch.
/// Returns `(pr_number, pr_url)` for the first result, or `None` when:
///   - the branch cannot be detected (detached HEAD, git unavailable)
///   - `gh pr list` fails (logged at warn)
///   - the response JSON is empty or unparseable
pub(crate) async fn fallback_find_pr_by_branch(project_root: &Path) -> Option<(u64, String)> {
    let branch_out = Command::new("git")
        .current_dir(project_root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !branch_out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_string();
    if branch.is_empty() || branch == "HEAD" {
        tracing::warn!("fallback_find_pr_by_branch: detached HEAD, cannot search by branch");
        return None;
    }

    let gh_out = Command::new("gh")
        .current_dir(project_root)
        .args([
            "pr",
            "list",
            "--search",
            &format!("head:{branch}"),
            "--state",
            "open",
            "--json",
            "number,url",
            "--limit",
            "5",
        ])
        .output()
        .await
        .ok()?;

    if !gh_out.status.success() {
        tracing::warn!(
            branch = %branch,
            stderr = %String::from_utf8_lossy(&gh_out.stderr).trim(),
            "fallback_find_pr_by_branch: gh pr list failed"
        );
        return None;
    }

    #[derive(serde::Deserialize)]
    struct PrItem {
        number: u64,
        url: String,
    }
    let items: Vec<PrItem> = match serde_json::from_slice(&gh_out.stdout) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(branch = %branch, error = %e, "fallback_find_pr_by_branch: JSON parse failed");
            return None;
        }
    };
    let first = items.into_iter().next()?;
    tracing::info!(
        branch = %branch,
        pr_number = first.number,
        pr_url = %first.url,
        "fallback_find_pr_by_branch: recovered PR via gh pr list"
    );
    Some((first.number, first.url))
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

/// Detect the `"owner/repo"` slug by scanning all configured git remotes.
///
/// Uses `git remote -v` (pure git, no GitHub auth required). Prefers `origin`
/// for stability but falls back to any other remote that has a recognisable
/// GitHub URL. Trying every remote prevents silently disabling the cross-repo
/// guard in repos or worktrees where the primary remote is named something
/// other than `origin` (e.g. `upstream`, `fork`, or a CI-injected name).
pub(crate) async fn detect_repo_slug(project: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(project)
        .args(["remote", "-v"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `git remote -v` output: "<name>\t<url> (fetch)\n<name>\t<url> (push)\n…"
    // Only inspect fetch lines to avoid processing each remote twice.
    let mut fallback: Option<String> = None;
    for line in stdout.lines() {
        if !line.ends_with("(fetch)") {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let name = match parts.next() {
            Some(n) => n.trim(),
            None => continue,
        };
        let rest = match parts.next() {
            Some(r) => r,
            None => continue,
        };
        let url = rest.trim_end_matches("(fetch)").trim();
        if let Some(slug) = parse_repo_slug_from_remote_url(url) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, body: &str) -> GhPrListItem {
        GhPrListItem {
            number: 1,
            head_ref_name: "feat/x".to_string(),
            url: "https://example.test/pr/1".to_string(),
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    // --- pr_claims_to_close_issue: positive cases ---

    #[test]
    fn title_suffix_alone_does_not_match() {
        // A PR title ending with `(#N)` but no GitHub closing keyword in title
        // or body is NOT a close signal. Human-authored PRs like
        // "docs: mention scheduler cleanup (#799)" would otherwise be falsely
        // reused as the branch for issue #799.
        let it = item("fix(dedup): guard against closed PRs (#791)", "");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn title_suffix_with_close_keyword_matches() {
        // A PR that has both the suffix AND a closing keyword in the body is fine.
        let it = item("fix(dedup): guard against closed PRs (#791)", "Fixes #791");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn closes_keyword_in_body_matches() {
        let it = item("some PR", "This PR closes #791 as agreed.");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn fixes_keyword_in_body_matches() {
        let it = item("some PR", "Fixes #791");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn resolves_keyword_case_insensitive() {
        let it = item("RESOLVES #791 finally", "");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn closes_with_colon_between_keyword_and_number() {
        // "closes: #791" — trim trailing colon before keyword check.
        let it = item("", "closes: #791");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    // --- pr_claims_to_close_issue: negative cases (the #799 bug) ---

    #[test]
    fn plain_mention_does_not_match() {
        // The exact scenario that caused #799: PR body merely references
        // another issue number as context, not as a close target.
        let it = item(
            "feat(scheduler): add periodic retry (#794)",
            "depends on #791 being fixed first",
        );
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn relates_to_does_not_match() {
        let it = item("some PR", "relates to #791");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn see_also_does_not_match() {
        let it = item("some PR", "See also #791 for context.");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn word_boundary_prefixes_does_not_match_fix() {
        // "prefixes #791" must NOT match the keyword "fixes".
        let it = item("some PR", "prefixes #791 with a label");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn different_issue_number_in_close_keyword_does_not_match() {
        // PR closes #100 but we're asking about #791 — must not match.
        let it = item("", "closes #100, mentions #791");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn title_suffix_for_other_issue_does_not_match() {
        let it = item("fix: something (#100)", "passing reference to #791");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn title_suffix_not_trailing_does_not_match() {
        // "(#791)" appears in the middle of the title but "#812" is the real
        // trailing harness suffix — must NOT match issue 791.
        let it = item("follow-up to prior fix (#791) (#812)", "");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn title_suffix_not_trailing_context_mention_does_not_match() {
        // "docs: mention prior fix (#791)" — no closing keyword, just a
        // mid-title mention that happens to end a sub-phrase.
        let it = item("docs: mention prior fix (#791) and move on (#812)", "");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn prefix_number_does_not_match_longer_issue() {
        // Codex-flagged regression: scanning for `#79` must NOT match `#791`.
        // A PR body "closes #791" claims to close issue 791, not issue 79.
        let it = item("", "closes #791");
        assert!(!pr_claims_to_close_issue(&it, 79, None));
    }

    #[test]
    fn exact_issue_still_matches_even_when_prefix_of_another() {
        // When asked about #79, a PR body that actually says "closes #79"
        // (e.g. followed by space/punctuation, not another digit) must match.
        let it = item("", "closes #79 and also mentions #791");
        assert!(pr_claims_to_close_issue(&it, 79, None));
    }

    // --- repo-qualified close references (Codex P2) ---

    #[test]
    fn repo_qualified_fixes_matches() {
        // GitHub allows "Fixes owner/repo#N" as a valid closing reference.
        let it = item("", "Fixes majiayu000/harness#791");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn repo_qualified_closes_matches() {
        let it = item("", "closes owner/repo#791 in this body");
        assert!(pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn repo_qualified_plain_mention_does_not_match() {
        // "related to owner/repo#N" must NOT match — no close keyword.
        let it = item("", "related to majiayu000/harness#791");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn repo_qualified_boundary_guards_longer_number() {
        // "fixes owner/repo#791" must not match when querying issue 79.
        let it = item("", "fixes owner/repo#791");
        assert!(!pr_claims_to_close_issue(&it, 79, None));
    }

    #[test]
    fn repo_qualified_cross_repo_does_not_match() {
        // A PR that closes rust-lang/rust#791 must not match issue #791 in our repo.
        let it = item("", "Fixes rust-lang/rust#791");
        assert!(!pr_claims_to_close_issue(
            &it,
            791,
            Some("majiayu000/harness")
        ));
    }

    #[test]
    fn repo_qualified_same_repo_matches_with_slug() {
        let it = item("", "Fixes majiayu000/harness#791");
        assert!(pr_claims_to_close_issue(
            &it,
            791,
            Some("majiayu000/harness")
        ));
    }

    #[test]
    fn cross_field_keyword_does_not_match() {
        // "fixes" at the end of the title must NOT pair with "#791" at the start
        // of the body. Scanning each field independently prevents this.
        let it = item("CI fixes", "#791 is the main bug");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn trailing_alpha_after_issue_number_does_not_match() {
        // "fixes #791abc" — alphanumeric character after the number must not match.
        let it = item("", "fixes #791abc");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    // --- word_boundary_before ---

    #[test]
    fn word_boundary_before_at_start_is_boundary() {
        assert!(word_boundary_before("fix", 3));
    }

    #[test]
    fn word_boundary_before_space_is_boundary() {
        assert!(word_boundary_before(" fix", 3));
    }

    #[test]
    fn word_boundary_before_alnum_is_not_boundary() {
        // "prefix" — char before last 3 ("fix") is 'e', alphanumeric.
        assert!(!word_boundary_before("prefix", 3));
    }

    #[test]
    fn word_boundary_before_multibyte_unicode_alnum_is_not_boundary() {
        // `é` is a multi-byte UTF-8 character (U+00E9, 0xC3 0xA9). A raw-byte
        // check would see 0xA9, which is not ASCII-alphanumeric, and falsely
        // report a boundary. The char-aware check must return false here.
        assert!(!word_boundary_before("éfix", 3));
    }

    #[test]
    fn unicode_prefix_does_not_match_keyword() {
        // "éfixes #791" — no word boundary before "fixes", must not match.
        let it = item("", "éfixes #791");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    #[test]
    fn cjk_prefix_does_not_match_keyword() {
        // "前fixes #791" — CJK character before "fixes". CJK chars are
        // alphanumeric under Unicode (letter category), so no boundary.
        let it = item("", "前fixes #791");
        assert!(!pr_claims_to_close_issue(&it, 791, None));
    }

    // --- parse_harness_mention_command (pre-existing, light coverage) ---

    // --- fallback_find_pr_by_branch JSON parsing tests (T6 / T7) ---

    #[test]
    fn fallback_json_valid_returns_first_item() {
        // T6: JSON returned by `gh pr list` with one match should deserialise correctly.
        #[derive(serde::Deserialize)]
        struct PrItem {
            number: u64,
            url: String,
        }
        let json = r#"[{"number":42,"url":"https://github.com/owner/repo/pull/42"}]"#;
        let items: Vec<PrItem> = serde_json::from_str(json).unwrap();
        let first = items.into_iter().next().unwrap();
        assert_eq!(first.number, 42);
        assert_eq!(first.url, "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn fallback_json_empty_array_returns_none() {
        // T7: empty JSON array — no PR found on this branch.
        #[derive(serde::Deserialize)]
        struct PrItem {
            #[allow(dead_code)]
            number: u64,
            #[allow(dead_code)]
            url: String,
        }
        let json = r#"[]"#;
        let items: Vec<PrItem> = serde_json::from_str(json).unwrap();
        assert!(items.into_iter().next().is_none());
    }

    #[test]
    fn parses_fix_ci_command() {
        assert_eq!(
            parse_harness_mention_command("@harness fix ci please"),
            Some(HarnessMentionCommand::FixCi)
        );
    }

    #[test]
    fn parses_review_command() {
        assert_eq!(
            parse_harness_mention_command("@harness review"),
            Some(HarnessMentionCommand::Review)
        );
    }

    #[test]
    fn parses_plain_mention() {
        assert_eq!(
            parse_harness_mention_command("hey @harness, take a look"),
            Some(HarnessMentionCommand::Mention)
        );
    }

    #[test]
    fn no_mention_returns_none() {
        assert_eq!(parse_harness_mention_command("nothing here"), None);
    }

    // --- parse_repo_slug_from_remote_url ---

    #[test]
    fn parses_ssh_remote() {
        assert_eq!(
            parse_repo_slug_from_remote_url("git@github.com:owner/repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn parses_https_remote() {
        assert_eq!(
            parse_repo_slug_from_remote_url("https://github.com/owner/repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn parses_https_remote_without_git_suffix() {
        assert_eq!(
            parse_repo_slug_from_remote_url("https://github.com/owner/repo"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn parses_ssh_scheme_remote() {
        // ssh://git@github.com/owner/repo.git — distinct from SCP-style
        // git@github.com:owner/repo.git; without this branch, detect_repo_slug
        // returns None and the cross-repo guard is silently disabled.
        assert_eq!(
            parse_repo_slug_from_remote_url("ssh://git@github.com/owner/repo.git"),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn rejects_unknown_remote() {
        assert_eq!(
            parse_repo_slug_from_remote_url("https://gitlab.com/owner/repo.git"),
            None
        );
    }
}
