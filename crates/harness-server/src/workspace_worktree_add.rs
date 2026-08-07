use super::*;

/// Failure classes for `git worktree add` (GH-1886). Each class carries a
/// stable string recorded in job-facing error messages so ops reviews can
/// aggregate failures by cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeAddFailureClass {
    /// A worktree admin entry (branch or path) is registered but its
    /// directory is gone — `git worktree prune` clears it.
    StaleWorktreeEntry,
    /// Another git process holds a lock in the admin area — transient.
    LockContention,
    /// The target path exists on disk and is not a registered worktree.
    PathCollision,
    /// The start ref does not resolve (fetch failed or ref deleted upstream).
    MissingRef,
    Unknown,
}

impl WorktreeAddFailureClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::StaleWorktreeEntry => "stale_worktree_entry",
            Self::LockContention => "lock_contention",
            Self::PathCollision => "path_collision",
            Self::MissingRef => "missing_ref",
            Self::Unknown => "unknown",
        }
    }
}

pub(crate) fn classify_worktree_add_stderr(stderr: &str) -> WorktreeAddFailureClass {
    let lower = stderr.to_lowercase();
    if lower.contains("missing but already registered")
        || lower.contains("missing but locked")
        || lower.contains("is already used by worktree")
        || lower.contains("is already checked out at")
    {
        WorktreeAddFailureClass::StaleWorktreeEntry
    } else if lower.contains("could not lock")
        || lower.contains("another git process")
        || (lower.contains("unable to create") && lower.contains(".lock"))
    {
        WorktreeAddFailureClass::LockContention
    } else if lower.contains("already exists") {
        WorktreeAddFailureClass::PathCollision
    } else if lower.contains("invalid reference")
        || lower.contains("not a valid ref")
        || lower.contains("unknown revision")
    {
        WorktreeAddFailureClass::MissingRef
    } else {
        WorktreeAddFailureClass::Unknown
    }
}

#[derive(Debug)]
pub(crate) struct WorktreeAddFailure {
    pub(crate) class: WorktreeAddFailureClass,
    pub(crate) attempts: u32,
    pub(crate) stderr: String,
}

impl WorktreeAddFailure {
    /// Render the queryable evidence suffix embedded in job-facing errors.
    pub(crate) fn evidence(&self) -> String {
        format!(
            "worktree_add_failure_class={} attempts={}: {}",
            self.class.as_str(),
            self.attempts,
            self.stderr
        )
    }
}

#[derive(Debug)]
pub(crate) enum WorktreeAddError {
    Spawn(std::io::Error),
    Failed(WorktreeAddFailure),
}

const WORKTREE_ADD_MAX_ATTEMPTS: u32 = 2;
const WORKTREE_ADD_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// Run `git worktree add -B <branch> <path> <start_ref>` with class-targeted
/// recovery (GH-1886): stale admin entries get one `git worktree prune` +
/// retry, lock contention gets one backoff + retry, other classes fail
/// immediately with the classified evidence.
pub(crate) async fn worktree_add_with_recovery(
    source_repo: &Path,
    branch: &str,
    workspace_path: &Path,
    start_ref: &str,
) -> Result<(), WorktreeAddError> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let output = git_command()
            .args([
                "-C",
                &source_repo.to_string_lossy(),
                "worktree",
                "add",
                "-B",
                branch,
                &workspace_path.to_string_lossy(),
                start_ref,
            ])
            .output()
            .await
            .map_err(WorktreeAddError::Spawn)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let class = classify_worktree_add_stderr(&stderr);
        if attempts >= WORKTREE_ADD_MAX_ATTEMPTS {
            return Err(WorktreeAddError::Failed(WorktreeAddFailure {
                class,
                attempts,
                stderr,
            }));
        }
        match class {
            WorktreeAddFailureClass::StaleWorktreeEntry => {
                tracing::warn!(
                    failure_class = class.as_str(),
                    branch,
                    "git worktree add hit a stale admin entry; pruning and retrying"
                );
                let prune = git_command()
                    .args(["-C", &source_repo.to_string_lossy(), "worktree", "prune"])
                    .output()
                    .await
                    .map_err(WorktreeAddError::Spawn)?;
                if !prune.status.success() {
                    return Err(WorktreeAddError::Failed(WorktreeAddFailure {
                        class,
                        attempts,
                        stderr: format!(
                            "{stderr}; git worktree prune also failed: {}",
                            String::from_utf8_lossy(&prune.stderr).trim()
                        ),
                    }));
                }
            }
            WorktreeAddFailureClass::LockContention => {
                tracing::warn!(
                    failure_class = class.as_str(),
                    branch,
                    "git worktree add hit lock contention; backing off and retrying"
                );
                tokio::time::sleep(WORKTREE_ADD_LOCK_RETRY_DELAY).await;
            }
            WorktreeAddFailureClass::PathCollision
            | WorktreeAddFailureClass::MissingRef
            | WorktreeAddFailureClass::Unknown => {
                return Err(WorktreeAddError::Failed(WorktreeAddFailure {
                    class,
                    attempts,
                    stderr,
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::{init_git_repo, run_git};
    use tempfile::TempDir;

    #[test]
    fn classifies_stale_entry_variants() {
        for stderr in [
            "fatal: 'harness/runtime-wf-github-issue-pr-81eb5207' is already used by worktree at '/tmp/wt'",
            "fatal: '/tmp/wt' is a missing but already registered worktree;\nuse 'add -f' to override, or 'prune' or 'remove' to clear",
            "fatal: 'main' is already checked out at '/tmp/other'",
        ] {
            assert_eq!(
                classify_worktree_add_stderr(stderr),
                WorktreeAddFailureClass::StaleWorktreeEntry,
                "stderr: {stderr}"
            );
        }
    }

    #[test]
    fn classifies_lock_path_and_ref_failures() {
        assert_eq!(
            classify_worktree_add_stderr(
                "fatal: Unable to create '/repo/.git/worktrees/x/index.lock': File exists.\n\nAnother git process seems to be running"
            ),
            WorktreeAddFailureClass::LockContention
        );
        assert_eq!(
            classify_worktree_add_stderr("fatal: '/tmp/wt' already exists"),
            WorktreeAddFailureClass::PathCollision
        );
        assert_eq!(
            classify_worktree_add_stderr("fatal: invalid reference: origin/main"),
            WorktreeAddFailureClass::MissingRef
        );
        assert_eq!(
            classify_worktree_add_stderr("fatal: something novel"),
            WorktreeAddFailureClass::Unknown
        );
    }

    #[tokio::test]
    async fn recovers_from_stale_worktree_entry_by_pruning() {
        let repo_dir = TempDir::new().expect("repo dir");
        init_git_repo(repo_dir.path());
        let repo = repo_dir.path().to_string_lossy().to_string();

        let stale_path = repo_dir.path().join("stale-wt");
        run_git(&[
            "-C",
            &repo,
            "worktree",
            "add",
            "-B",
            "harness/stale-task",
            &stale_path.to_string_lossy(),
            "HEAD",
        ]);
        // Delete the worktree directory without deregistering it: the branch
        // stays "used by" a worktree whose directory is gone.
        std::fs::remove_dir_all(&stale_path).expect("remove stale worktree dir");

        let new_path = repo_dir.path().join("fresh-wt");
        worktree_add_with_recovery(repo_dir.path(), "harness/stale-task", &new_path, "HEAD")
            .await
            .expect("prune + retry should recover the stale entry");
        assert!(new_path.join(".git").exists());
    }

    #[tokio::test]
    async fn missing_ref_fails_without_retry_and_carries_evidence() {
        let repo_dir = TempDir::new().expect("repo dir");
        init_git_repo(repo_dir.path());
        let new_path = repo_dir.path().join("wt");

        let err = worktree_add_with_recovery(
            repo_dir.path(),
            "harness/task",
            &new_path,
            "origin/does-not-exist",
        )
        .await
        .expect_err("missing ref must fail");
        match err {
            WorktreeAddError::Failed(failure) => {
                assert_eq!(failure.class, WorktreeAddFailureClass::MissingRef);
                assert_eq!(failure.attempts, 1);
                assert!(failure
                    .evidence()
                    .starts_with("worktree_add_failure_class=missing_ref attempts=1:"));
            }
            other => panic!("expected classified failure, got {other:?}"),
        }
    }
}
