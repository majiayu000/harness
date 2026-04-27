//! Assess the rebase state and conflict size of a PR without modifying any
//! repository state.
//!
//! Read-only probes use `git rev-list`, `git merge-tree`, and `gh` queries.
//! Conflict region counting uses `git merge-tree` (a dry-run three-way merge
//! that writes nothing to the working tree or index) so that `<<<<<<<` markers
//! reflect the actual merge result against `origin/main`, not the PR's own diff.
//! Write operations (rebase, force-push) are delegated to the agent via the
//! prompt returned by [`harness_core::prompts::rebase_conflicting_pr`].

use crate::task_runner::{mutate_and_persist, TaskId, TaskStatus, TaskStore};
use harness_core::agent::CodeAgent;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::Duration;

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

// ── Proactive rebase gate ────────────────────────────────────────────────────

/// Rebase state of a PR branch relative to `origin/main`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrRebaseState {
    /// Branch tip equals `origin/main` HEAD — no rebase needed.
    UpToDate,
    /// Branch is behind `origin/main` but a dry-run three-way merge produces
    /// no conflict markers — a clean rebase is possible.
    BehindClean { commits_behind: u32 },
    /// A dry-run three-way merge produces conflict markers — manual
    /// intervention is required.
    Conflicted {
        file_count: usize,
        region_count: usize,
    },
    /// A command invocation failed; the error message is included.
    Unknown(String),
}

/// Event name emitted when the branch is already at `origin/main` tip.
pub(crate) const PR_REBASE_UP_TO_DATE: &str = "pr_rebase_up_to_date";
/// Event name emitted after the agent successfully rebased and force-pushed.
pub(crate) const PR_REBASE_SUCCESS: &str = "pr_rebase_success";
/// Event name emitted when the branch has unresolvable merge conflicts or when
/// the rebase agent turn did not report `REBASE_OK`.
pub(crate) const PR_REBASE_CONFLICT: &str = "pr_rebase_conflict";

/// Pure classification of a `git merge-tree` output into a [`PrRebaseState`].
/// Extracted so it can be unit-tested without running git commands.
fn classify_rebase_state(commits_behind: u32, merge_tree_output: &str) -> PrRebaseState {
    if commits_behind == 0 {
        return PrRebaseState::UpToDate;
    }
    let file_count = merge_tree_output
        .lines()
        .filter(|l| *l == "changed in both")
        .count();
    let region_count = merge_tree_output
        .lines()
        .filter(|l| l.starts_with("<<<<<<<"))
        .count();
    if file_count == 0 && region_count == 0 {
        PrRebaseState::BehindClean { commits_behind }
    } else {
        PrRebaseState::Conflicted {
            file_count,
            region_count,
        }
    }
}

/// Probe the rebase state of `pr_num` relative to `origin/main` without
/// modifying any repository state.
///
/// Uses read-only git commands:
/// 1. Fetch the branch name via the GitHub API.
/// 2. `git fetch origin` to update remote refs (best-effort).
/// 3. `git rev-list --count origin/<branch>..origin/main` to detect lag.
/// 4. `git merge-tree` dry-run to classify conflicts.
pub(crate) async fn check_pr_needs_rebase(pr_num: u64, project: &Path) -> PrRebaseState {
    let branch = match gh_head_ref_name(pr_num, project).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(pr = pr_num, error = %e, "check_pr_needs_rebase: failed to get branch name");
            return PrRebaseState::Unknown(e);
        }
    };

    if !is_safe_ref(&branch) {
        let msg = format!("PR #{pr_num}: branch `{branch}` contains unsafe characters");
        tracing::error!(pr = pr_num, branch = %branch, "{msg}");
        return PrRebaseState::Unknown(msg);
    }

    // Fetch to ensure remote refs are current (best-effort; a stale fetch is
    // not fatal — rev-list and merge-tree will still run on cached refs).
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

    // Count commits in origin/main that are NOT yet in origin/<branch>.
    // A result of 0 means the branch is at (or ahead of) origin/main.
    let commits_behind = {
        let rev_out = match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            Command::new("git")
                .current_dir(project)
                .args([
                    "rev-list",
                    "--count",
                    &format!("origin/{branch}..origin/main"),
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output(),
        )
        .await
        {
            Ok(Ok(o)) if o.status.success() => o,
            Ok(Ok(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                tracing::error!(pr = pr_num, stderr = %stderr, "git rev-list failed");
                return PrRebaseState::Unknown(format!(
                    "git rev-list for PR #{pr_num} failed: {stderr}"
                ));
            }
            Ok(Err(e)) => {
                return PrRebaseState::Unknown(format!("git rev-list for PR #{pr_num}: {e}"))
            }
            Err(_) => {
                return PrRebaseState::Unknown(format!("git rev-list timed out for PR #{pr_num}"))
            }
        };
        String::from_utf8_lossy(&rev_out.stdout)
            .trim()
            .parse::<u32>()
            .unwrap_or(0)
    };

    if commits_behind == 0 {
        return PrRebaseState::UpToDate;
    }

    // Branch is behind — check whether a rebase would be clean by doing a
    // dry-run three-way merge via `git merge-tree`.
    let base_sha = {
        let base_out = match tokio::time::timeout(
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
        {
            Ok(Ok(o)) if o.status.success() => o,
            Ok(Ok(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                return PrRebaseState::Unknown(format!(
                    "git merge-base for PR #{pr_num} failed: {stderr}"
                ));
            }
            Ok(Err(e)) => {
                return PrRebaseState::Unknown(format!("git merge-base for PR #{pr_num}: {e}"))
            }
            Err(_) => {
                return PrRebaseState::Unknown(format!("git merge-base timed out for PR #{pr_num}"))
            }
        };
        String::from_utf8_lossy(&base_out.stdout).trim().to_string()
    };

    let merge_out = match tokio::time::timeout(
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
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return PrRebaseState::Unknown(format!("git merge-tree for PR #{pr_num}: {e}"))
        }
        Err(_) => {
            return PrRebaseState::Unknown(format!("git merge-tree timed out for PR #{pr_num}"))
        }
    };

    let text = String::from_utf8_lossy(&merge_out.stdout);
    classify_rebase_state(commits_behind, &text)
}

/// Outcome of the [`run_pr_rebase_gate`] check.
pub(crate) enum PrRebaseGateOutcome {
    /// Rebase was either not needed or succeeded; the caller should proceed.
    /// `rebase_pushed` is `true` when the agent force-pushed a new commit.
    Proceed { rebase_pushed: bool },
    /// Merge conflicts prevent rebase; the task has been marked `Failed`.
    TaskFailed,
    /// A command invocation failed; fall back to the legacy conflict gate.
    Unknown,
}

/// Run the proactive rebase gate for a fresh `pr:N` task.
///
/// Probes the branch state with [`check_pr_needs_rebase`], then:
/// - `UpToDate` → emits `pr_rebase_up_to_date`, returns `Proceed{false}`.
/// - `BehindClean` → delegates rebase to the agent; emits `pr_rebase_success`
///   on `REBASE_OK` and returns `Proceed{true}`; emits `pr_rebase_conflict`
///   and marks the task `Failed` otherwise.
/// - `Conflicted` → emits `pr_rebase_conflict`, marks task `Failed`.
/// - `Unknown` → logs an error event, returns `Unknown` so the caller can
///   fall back to the legacy [`assess_pr_conflict`] gate.
///
/// Call this **before** the legacy [`assess_pr_conflict`] gate.
pub(crate) async fn run_pr_rebase_gate(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    pr_num: u64,
    project: &Path,
    repo_slug: &str,
    turn_timeout: Duration,
    cargo_env: &HashMap<String, String>,
    events: &Arc<harness_observe::event_store::EventStore>,
) -> anyhow::Result<PrRebaseGateOutcome> {
    let state = check_pr_needs_rebase(pr_num, project).await;

    match state {
        PrRebaseState::UpToDate => {
            tracing::info!(pr = pr_num, "rebase gate: branch up to date — proceeding");
            let mut ev = harness_core::types::Event::new(
                harness_core::types::SessionId::new(),
                PR_REBASE_UP_TO_DATE,
                "pr_rebase_gate",
                harness_core::types::Decision::Pass,
            );
            ev.detail = Some(format!("pr:{pr_num} is already at origin/main HEAD"));
            if let Err(e) = events.log(&ev).await {
                tracing::warn!("failed to log {PR_REBASE_UP_TO_DATE} event: {e}");
            }
            Ok(PrRebaseGateOutcome::Proceed {
                rebase_pushed: false,
            })
        }

        PrRebaseState::BehindClean { commits_behind } => {
            tracing::info!(
                pr = pr_num,
                commits_behind,
                "rebase gate: branch is behind main — attempting agent rebase"
            );
            let pushed = super::triage_pipeline::run_rebase_turn(
                agent,
                pr_num,
                project,
                repo_slug,
                turn_timeout,
                cargo_env,
            )
            .await;

            if pushed {
                let mut ev = harness_core::types::Event::new(
                    harness_core::types::SessionId::new(),
                    PR_REBASE_SUCCESS,
                    "pr_rebase_gate",
                    harness_core::types::Decision::Pass,
                );
                ev.detail = Some(format!(
                    "pr:{pr_num} rebased onto origin/main ({commits_behind} commits behind)"
                ));
                if let Err(e) = events.log(&ev).await {
                    tracing::warn!("failed to log {PR_REBASE_SUCCESS} event: {e}");
                }
                Ok(PrRebaseGateOutcome::Proceed {
                    rebase_pushed: true,
                })
            } else {
                let detail = format!(
                    "pr:{pr_num} is behind main by {commits_behind} commits and rebase agent did not report REBASE_OK"
                );
                let mut ev = harness_core::types::Event::new(
                    harness_core::types::SessionId::new(),
                    PR_REBASE_CONFLICT,
                    "pr_rebase_gate",
                    harness_core::types::Decision::Block,
                );
                ev.detail = Some(detail);
                if let Err(e) = events.log(&ev).await {
                    tracing::warn!("failed to log {PR_REBASE_CONFLICT} event: {e}");
                }
                mutate_and_persist(store, task_id, |s| {
                    s.status = TaskStatus::Failed;
                    s.error = Some(format!(
                        "pr:{pr_num} is behind main by {commits_behind} commits and rebase was not pushed; manual resolution required"
                    ));
                })
                .await?;
                Ok(PrRebaseGateOutcome::TaskFailed)
            }
        }

        PrRebaseState::Conflicted {
            file_count,
            region_count,
        } => {
            tracing::warn!(
                pr = pr_num,
                file_count,
                region_count,
                "rebase gate: merge conflicts detected — failing task"
            );
            let detail = format!(
                "pr:{pr_num} has rebase conflicts: {file_count} file(s), {region_count} region(s); manual resolution required"
            );
            let mut ev = harness_core::types::Event::new(
                harness_core::types::SessionId::new(),
                PR_REBASE_CONFLICT,
                "pr_rebase_gate",
                harness_core::types::Decision::Block,
            );
            ev.detail = Some(detail.clone());
            if let Err(e) = events.log(&ev).await {
                tracing::warn!("failed to log {PR_REBASE_CONFLICT} event: {e}");
            }
            mutate_and_persist(store, task_id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(detail);
            })
            .await?;
            Ok(PrRebaseGateOutcome::TaskFailed)
        }

        PrRebaseState::Unknown(e) => {
            // Emit an observable warning event (U-29) before falling back.
            tracing::error!(
                pr = pr_num,
                error = %e,
                "rebase gate: command failure — falling back to legacy conflict gate"
            );
            let mut ev = harness_core::types::Event::new(
                harness_core::types::SessionId::new(),
                PR_REBASE_CONFLICT,
                "pr_rebase_gate",
                harness_core::types::Decision::Warn,
            );
            ev.detail = Some(format!("pr:{pr_num}: rebase check command failed: {e}"));
            if let Err(e2) = events.log(&ev).await {
                tracing::warn!("failed to log {PR_REBASE_CONFLICT} warn event: {e2}");
            }
            Ok(PrRebaseGateOutcome::Unknown)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for the PrConflictSize threshold logic — does not invoke `gh`.

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

    // Unit tests for PrRebaseState classification — does not invoke git.

    #[test]
    fn zero_commits_behind_is_up_to_date() {
        assert_eq!(classify_rebase_state(0, ""), PrRebaseState::UpToDate);
        assert_eq!(
            classify_rebase_state(0, "changed in both\n<<<<<<< HEAD"),
            PrRebaseState::UpToDate
        );
    }

    #[test]
    fn nonzero_behind_no_markers_is_behind_clean() {
        assert_eq!(
            classify_rebase_state(3, "just some regular diff content"),
            PrRebaseState::BehindClean { commits_behind: 3 }
        );
        assert_eq!(
            classify_rebase_state(1, ""),
            PrRebaseState::BehindClean { commits_behind: 1 }
        );
    }

    #[test]
    fn conflict_markers_in_merge_tree_is_conflicted() {
        let output = "changed in both\n<<<<<<< HEAD\nfoo\n=======\nbar\n>>>>>>> branch";
        assert_eq!(
            classify_rebase_state(2, output),
            PrRebaseState::Conflicted {
                file_count: 1,
                region_count: 1
            }
        );
    }

    #[test]
    fn multiple_conflict_regions_counted() {
        let output = "changed in both\n<<<<<<< HEAD\nfoo\n=======\nbar\n>>>>>>> branch\n\
                      changed in both\n<<<<<<< HEAD\nbaz\n=======\nqux\n>>>>>>> branch";
        assert_eq!(
            classify_rebase_state(5, output),
            PrRebaseState::Conflicted {
                file_count: 2,
                region_count: 2
            }
        );
    }

    #[test]
    fn pr_rebase_event_constants_are_distinct_and_nonempty() {
        assert!(!PR_REBASE_UP_TO_DATE.is_empty());
        assert!(!PR_REBASE_SUCCESS.is_empty());
        assert!(!PR_REBASE_CONFLICT.is_empty());
        assert_ne!(PR_REBASE_UP_TO_DATE, PR_REBASE_SUCCESS);
        assert_ne!(PR_REBASE_SUCCESS, PR_REBASE_CONFLICT);
        assert_ne!(PR_REBASE_UP_TO_DATE, PR_REBASE_CONFLICT);
    }
}
