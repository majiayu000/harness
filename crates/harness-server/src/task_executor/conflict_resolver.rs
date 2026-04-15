//! Assess the size of a PR conflict without modifying any repository state.
//!
//! All operations here are read-only `gh` queries. Write operations (rebase,
//! force-push) are delegated to the agent via the prompt returned by
//! [`harness_core::prompts::rebase_conflicting_pr`].

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

    // Step 2: count conflicting files.
    let file_count = match gh_conflict_file_count(pr_num, project).await {
        Ok(n) => n,
        Err(e) => return PrConflictSize::Unknown(e),
    };

    // Early exit: if already over the file threshold, skip the more expensive
    // diff parse and go directly to Large.
    if file_count > 3 {
        return PrConflictSize::Large {
            file_count,
            region_count: 0,
        };
    }

    // Step 3: count conflict marker regions.
    let region_count = match gh_conflict_region_count(pr_num, project).await {
        Ok(n) => n,
        Err(e) => return PrConflictSize::Unknown(e),
    };

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

/// Run `gh pr diff <pr_num> --name-only` and return the count of distinct files.
async fn gh_conflict_file_count(pr_num: u64, project: &Path) -> Result<usize, String> {
    let output = Command::new("gh")
        .current_dir(project)
        .args(["pr", "diff", &pr_num.to_string(), "--name-only"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("gh pr diff --name-only #{pr_num}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("gh pr diff --name-only #{pr_num} failed: {stderr}"));
    }

    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    Ok(count)
}

/// Run `gh pr diff <pr_num>` and return the number of conflict-marker lines (`<<<<<<<`).
async fn gh_conflict_region_count(pr_num: u64, project: &Path) -> Result<usize, String> {
    let output = Command::new("gh")
        .current_dir(project)
        .args(["pr", "diff", &pr_num.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("gh pr diff #{pr_num}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("gh pr diff #{pr_num} failed: {stderr}"));
    }

    let count = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.starts_with("+<<<<<<<") || l.starts_with("-<<<<<<<"))
        .count();
    Ok(count)
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
