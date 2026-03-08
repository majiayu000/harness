use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

fn worktree_root(base: &Path) -> PathBuf {
    base.join(".harness").join("worktrees")
}

fn worktree_path(base: &Path, task_id: &str) -> PathBuf {
    worktree_root(base).join(task_id)
}

fn worktree_branch(task_id: &str) -> String {
    format!("task/{task_id}")
}

async fn run_git(base: &Path, args: &[&str]) -> Result<std::process::Output> {
    let output = Command::new("git")
        .arg("-C")
        .arg(base)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("failed to run git {:?}: {e}", args))?;
    Ok(output)
}

/// Create per-task isolated git worktree under `<base>/.harness/worktrees/<task_id>`.
pub async fn create_worktree(base: &Path, task_id: &str) -> Result<PathBuf> {
    let wt_root = worktree_root(base);
    let wt_path = worktree_path(base, task_id);
    let branch = worktree_branch(task_id);

    tokio::fs::create_dir_all(&wt_root)
        .await
        .map_err(|e| anyhow!("failed to create worktree root {}: {e}", wt_root.display()))?;

    let output = run_git(
        base,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            wt_path.to_string_lossy().as_ref(),
        ],
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow!(
            "git worktree add failed for task {}: {}",
            task_id,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(wt_path)
}

/// Remove per-task worktree and its branch.
pub async fn remove_worktree(base: &Path, task_id: &str) -> Result<()> {
    let wt_path = worktree_path(base, task_id);
    let branch = worktree_branch(task_id);

    let rm_output = run_git(
        base,
        &[
            "worktree",
            "remove",
            "--force",
            wt_path.to_string_lossy().as_ref(),
        ],
    )
    .await?;
    if !rm_output.status.success() {
        let stderr = String::from_utf8_lossy(&rm_output.stderr);
        // Best effort when worktree is already gone.
        if !stderr.contains("not found") && !stderr.contains("is not a working tree") {
            return Err(anyhow!(
                "git worktree remove failed for task {}: {}",
                task_id,
                stderr
            ));
        }
    }

    let branch_output = run_git(base, &["branch", "-D", &branch]).await?;
    if !branch_output.status.success() {
        let stderr = String::from_utf8_lossy(&branch_output.stderr);
        // Best effort when branch is already gone.
        if !stderr.contains("not found") && !stderr.contains("not exist") {
            return Err(anyhow!(
                "git branch -D failed for task {}: {}",
                task_id,
                stderr
            ));
        }
    }

    if wt_path.exists() {
        tokio::fs::remove_dir_all(&wt_path).await.map_err(|e| {
            anyhow!(
                "failed to cleanup worktree directory {}: {e}",
                wt_path.display()
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn run_git_sync(base: &Path, args: &[&str]) -> anyhow::Result<()> {
        let output = StdCommand::new("git")
            .arg("-C")
            .arg(base)
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn init_repo(base: &Path) -> anyhow::Result<()> {
        run_git_sync(base, &["init"])?;
        run_git_sync(base, &["config", "user.email", "harness-test@example.com"])?;
        run_git_sync(base, &["config", "user.name", "Harness Test"])?;
        std::fs::write(base.join("README.md"), "test\n")?;
        run_git_sync(base, &["add", "README.md"])?;
        run_git_sync(base, &["commit", "-m", "init"])?;
        Ok(())
    }

    #[tokio::test]
    async fn create_and_remove_worktree_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;
        let task_id = "test-task-1";

        let wt = create_worktree(dir.path(), task_id).await?;
        assert!(wt.exists(), "worktree path should exist");
        assert!(
            wt.join(".git").exists(),
            "worktree should be a git checkout"
        );

        remove_worktree(dir.path(), task_id).await?;
        assert!(!wt.exists(), "worktree path should be removed");

        Ok(())
    }

    #[tokio::test]
    async fn two_worktrees_can_coexist() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path())?;

        let (wt1, wt2) = tokio::join!(
            create_worktree(dir.path(), "task-a"),
            create_worktree(dir.path(), "task-b"),
        );
        let wt1 = wt1?;
        let wt2 = wt2?;

        assert!(wt1.exists());
        assert!(wt2.exists());
        assert_ne!(wt1, wt2);

        remove_worktree(dir.path(), "task-a").await?;
        remove_worktree(dir.path(), "task-b").await?;
        assert!(!wt1.exists());
        assert!(!wt2.exists());

        Ok(())
    }
}
