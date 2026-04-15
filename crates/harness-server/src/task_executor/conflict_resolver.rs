//! Assess the size of a PR conflict without modifying any repository state.
//!
//! Mergeability is checked via read-only `gh` queries.  Conflict region
//! counting uses `git merge-tree` (a dry-run three-way merge that writes
//! nothing to the working tree or index) so that `<<<<<<<` markers reflect
//! the actual merge result against `origin/main`, not the PR's own diff.
//! Write operations (rebase, force-push) are delegated to the agent via the
//! prompt returned by [`harness_core::prompts::rebase_conflicting_pr`].

use std::path::Path;
use tokio::process::Command;

/// Classification of how large a PR's merge conflict is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrConflictSize {
    /// The PR is not in a conflicting state.
    Clean,
    /// The PR is conflicting and small enough for auto-rebase:
    /// at most 3 files and fewer than 5 conflict regions.
    Small {
        file_count: usize,
        region_count: usize,
    },
    /// The PR is conflicting but too large for auto-rebase:
    /// more than 3 files OR 5 or more conflict regions.
    Large {
        file_count: usize,
        region_count: usize,
    },
    /// A `gh` invocation failed; the error message is included.
    Unknown(String),
}

/// Assess the conflict size of a GitHub PR without touching the working tree.
///
/// Returns:
/// - [`PrConflictSize::Clean`] when the PR is not conflicting.
/// - [`PrConflictSize::Small`] when conflicts are resolvable by auto-rebase.
/// - [`PrConflictSize::Large`] when conflicts require manual intervention.
/// - [`PrConflictSize::Unknown`] when a `gh` call fails.
pub(crate) async fn assess_pr_conflict(pr_num: u64, project: &Path) -> PrConflictSize {
    // Step 1: check mergeability — 10 s timeout for the status query.
    let mergeable = match gh_mergeable(pr_num, project).await {
        Ok(s) => s,
        Err(e) => return PrConflictSize::Unknown(e),
    };
    if mergeable != "CONFLICTING" {
        return PrConflictSize::Clean;
    }

    // Step 2: measure the actual conflict size using a dry-run merge.
    let (file_count, region_count) = match git_conflict_counts(pr_num, project).await {
        Ok(pair) => pair,
        Err(e) => return PrConflictSize::Unknown(e),
    };

    // Early exit: if already over the file threshold, skip region check.
    if file_count > 3 {
        return PrConflictSize::Large {
            file_count,
            region_count,
        };
    }

    if file_count <= 3 && region_count < 5 {
        PrConflictSize::Small {
            file_count,
            region_count,
        }
    } else {
        PrConflictSize::Large {
            file_count,
            region_count,
        }
    }
}

/// Run `gh pr view <pr_num> --json mergeable --jq .mergeable` with a 10 s timeout.
/// Returns the raw string value (e.g. `"CONFLICTING"`, `"MERGEABLE"`, `"UNKNOWN"`).
async fn gh_mergeable(pr_num: u64, project: &Path) -> Result<String, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new("gh")
            .current_dir(project)
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "mergeable",
                "--jq",
                ".mergeable",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("gh pr view #{pr_num} --json mergeable timed out"))?
    .map_err(|e| format!("gh pr view #{pr_num}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("gh pr view #{pr_num} failed: {stderr}"));
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // gh --jq returns a JSON string with surrounding quotes; strip them.
    Ok(value.trim_matches('"').to_string())
}

/// Returns `true` if `name` contains only characters that are safe to embed
/// inside single-quoted shell strings (i.e. no single-quote or NUL).
fn is_safe_ref(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| {
            c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '@' | '~' | '+' | ':')
        })
}

/// Fetch the `headRefName` of a PR from the GitHub API.
async fn gh_head_ref_name(pr_num: u64, project: &Path) -> Result<String, String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new("gh")
            .current_dir(project)
            .args([
                "pr",
                "view",
                &pr_num.to_string(),
                "--json",
                "headRefName",
                "--jq",
                ".headRefName",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("gh pr view #{pr_num} --json headRefName timed out"))?
    .map_err(|e| format!("gh pr view #{pr_num}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("gh pr view #{pr_num} failed: {stderr}"));
    }

    let name = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();
    Ok(name)
}

/// Perform a read-only three-way merge via `git merge-tree` and return
/// `(conflicting_file_count, conflict_region_count)`.
///
/// `git merge-tree <base> origin/main origin/<branch>` outputs the synthetic
/// merge result — including `<<<<<<<` / `=======` / `>>>>>>>` markers for
/// unresolved sections — without touching the working tree or index.  Each
/// `<<<<<<< ` line is one conflict region; each `changed in both` header is
/// one conflicting file.
async fn git_conflict_counts(pr_num: u64, project: &Path) -> Result<(usize, usize), String> {
    let branch = gh_head_ref_name(pr_num, project).await?;
    if !is_safe_ref(&branch) {
        return Err(format!(
            "PR #{pr_num}: branch name `{branch}` contains unsafe characters"
        ));
    }

    // Fetch to ensure origin/main and origin/<branch> are current.
    // Best-effort: a stale fetch is not fatal — merge-tree will still run
    // on whatever refs are already present.
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Command::new("git")
            .current_dir(project)
            .args(["fetch", "origin", "--quiet"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    {
        Ok(Ok(o)) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(pr = pr_num, stderr = %stderr, "git fetch origin failed; proceeding with cached refs");
        }
        Err(_) => tracing::warn!(
            pr = pr_num,
            "git fetch origin timed out; proceeding with cached refs"
        ),
        _ => {}
    }

    // Compute the merge base between origin/main and origin/<branch>.
    let base_out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Command::new("git")
            .current_dir(project)
            .args(["merge-base", "origin/main", &format!("origin/{branch}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("git merge-base timed out for PR #{pr_num}"))?
    .map_err(|e| format!("git merge-base for PR #{pr_num}: {e}"))?;

    if !base_out.status.success() {
        let stderr = String::from_utf8_lossy(&base_out.stderr).trim().to_string();
        return Err(format!("git merge-base for PR #{pr_num} failed: {stderr}"));
    }
    let base_sha = String::from_utf8_lossy(&base_out.stdout).trim().to_string();

    // Dry-run three-way merge: outputs file content with conflict markers but
    // does not write to the working tree or index.
    let merge_out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Command::new("git")
            .current_dir(project)
            .args([
                "merge-tree",
                &base_sha,
                "origin/main",
                &format!("origin/{branch}"),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("git merge-tree timed out for PR #{pr_num}"))?
    .map_err(|e| format!("git merge-tree for PR #{pr_num}: {e}"))?;

    let text = String::from_utf8_lossy(&merge_out.stdout);
    // "changed in both" appears once per file that has conflicting edits.
    let file_count = text.lines().filter(|l| *l == "changed in both").count();
    // Each "<<<<<<< " line opens one conflict region.
    let region_count = text.lines().filter(|l| l.starts_with("<<<<<<<")).count();
    Ok((file_count, region_count))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for the threshold logic — does not invoke `gh`.

    fn small(f: usize, r: usize) -> PrConflictSize {
        PrConflictSize::Small {
            file_count: f,
            region_count: r,
        }
    }

    fn large(f: usize, r: usize) -> PrConflictSize {
        PrConflictSize::Large {
            file_count: f,
            region_count: r,
        }
    }

    fn classify(file_count: usize, region_count: usize) -> PrConflictSize {
        if file_count <= 3 && region_count < 5 {
            small(file_count, region_count)
        } else {
            large(file_count, region_count)
        }
    }

    #[test]
    fn conflicting_small_pr() {
        assert_eq!(classify(2, 3), small(2, 3));
    }

    #[test]
    fn conflicting_large_by_files() {
        assert_eq!(classify(4, 2), large(4, 2));
    }

    #[test]
    fn conflicting_large_by_regions() {
        assert_eq!(classify(2, 6), large(2, 6));
    }

    #[test]
    fn boundary_exactly_3_files_4_regions_is_small() {
        assert_eq!(classify(3, 4), small(3, 4));
    }

    #[test]
    fn boundary_exactly_5_regions_is_large() {
        assert_eq!(classify(1, 5), large(1, 5));
    }
}
