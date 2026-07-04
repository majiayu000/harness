use super::*;

pub(super) fn is_valid_branch_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('-') || name.contains("..") {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'/' || b == b'-' || b == b'_' || b == b'.')
}

pub(super) fn sanitize_task_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Sanitize a GitHub repository slug for use as a filesystem path component.
///
/// Preserves underscores, dots, and hyphens (all valid in repo names) so that
/// `my.org/repo` and `my_org/repo` produce distinct keys (`my.org_repo` vs
/// `my_org_repo`). The `/` org-repo separator maps to `_`. GitHub organisation
/// names cannot contain underscores (only `[a-zA-Z0-9-]`), so the `owner_repo`
/// output is unambiguous for valid GitHub slugs.
pub(crate) fn sanitize_repo_slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Return an 8-character lowercase hex string from a 32-bit FNV-1a hash of `s`.
///
/// This is a deterministic, stable hash with no external dependencies, used to
/// produce a unique project scope component in deterministic workspace keys.
pub(crate) fn fnv1a_8(s: &str) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for b in s.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{hash:08x}")
}

/// Derive the filesystem key for a workspace.
///
/// For tasks with `external_id` matching `issue:N` or `pr:N` and a non-empty `repo`,
/// returns `<path_hash>__<sanitized_repo>__<sanitized_external_id>`
/// (e.g. `a3f2b1c4__myorg_my-repo__issue_42`), scoped by a hash of the project's
/// absolute path so that two different projects targeting the same GitHub repo/issue
/// do not collide even when their directory names are identical.
/// Falls back to the UUID-derived key when `external_id`/`repo` are absent or don't match.
pub(super) fn derive_workspace_key(
    task_id: &TaskId,
    external_id: Option<&str>,
    repo: Option<&str>,
    source_repo: Option<&std::path::Path>,
) -> String {
    if let (Some(eid), Some(r)) = (external_id, repo) {
        if !r.is_empty() && is_issue_or_pr_id(eid) {
            let project_prefix = source_repo
                .map(|p| {
                    let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                    format!("{}__", fnv1a_8(&canonical.to_string_lossy()))
                })
                .unwrap_or_default();
            return format!(
                "{}{}__{}",
                project_prefix,
                sanitize_repo_slug(r),
                sanitize_task_id(eid)
            );
        }
    }
    sanitize_task_id(&task_id.0)
}

pub(super) fn is_issue_or_pr_id(s: &str) -> bool {
    let digits = if let Some(rest) = s.strip_prefix("issue:") {
        rest
    } else if let Some(rest) = s.strip_prefix("pr:") {
        rest
    } else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

pub(super) fn owner_record_external_id(record: &WorkspaceOwnerRecord) -> Option<String> {
    let (issue, pr) = crate::reconciliation::parse_external_id(Some(&record.task_id));
    if issue.is_some() || pr.is_some() {
        return Some(record.task_id.clone());
    }
    record
        .workspace_key
        .as_deref()
        .and_then(external_id_from_workspace_key)
}

pub(super) fn external_id_from_workspace_key(key: &str) -> Option<String> {
    let suffix = key.rsplit("__").next()?;
    if let Some(issue) = suffix.strip_prefix("issue_") {
        if !issue.is_empty() && issue.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("issue:{issue}"));
        }
    }
    if let Some(pr) = suffix.strip_prefix("pr_") {
        if !pr.is_empty() && pr.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("pr:{pr}"));
        }
    }
    None
}

pub(super) fn repo_slug_from_workspace_key(key: &str) -> Option<String> {
    let mut parts = key.rsplit("__");
    let _external_id = parts.next()?;
    let repo_part = parts.next()?;
    let (owner, repo) = repo_part.split_once('_')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Returns true when the git worktree at `path` is currently on `branch`.
/// Used to distinguish crash-recovery (same task's worktree) from a true collision.
pub(crate) async fn run_hook(script: &str, cwd: &Path) -> anyhow::Result<()> {
    crate::post_validator::validate_command_safety(script).map_err(|e| anyhow::anyhow!("{e}"))?;
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "hook exited with status {:?}: {}",
            output.status.code(),
            stderr.trim()
        );
    }
    Ok(())
}

pub(super) fn workspace_git_dir(workspace_path: &Path) -> anyhow::Result<PathBuf> {
    let dot_git = workspace_path.join(".git");
    let metadata = std::fs::metadata(&dot_git)?;
    if metadata.is_dir() {
        return Ok(dot_git);
    }

    let gitdir = std::fs::read_to_string(&dot_git)?;
    let relative = gitdir
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("invalid gitdir metadata at {:?}", dot_git))?;
    let gitdir_path = Path::new(relative);
    Ok(if gitdir_path.is_absolute() {
        gitdir_path.to_path_buf()
    } else {
        workspace_path.join(gitdir_path)
    })
}

pub(super) fn owner_record_path(workspace_path: &Path) -> anyhow::Result<PathBuf> {
    Ok(workspace_git_dir(workspace_path)?.join(OWNER_RECORD_FILE))
}

pub(super) fn read_owner_record(workspace_path: &Path) -> Option<WorkspaceOwnerRecord> {
    let bytes = std::fs::read(owner_record_path(workspace_path).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn task_summary_workspace_path(root: &Path, task: &TaskSummary) -> PathBuf {
    task.workspace_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(sanitize_task_id(&task.id.0)))
}

pub(super) fn write_owner_record(
    workspace_path: &Path,
    owner_record: &WorkspaceOwnerRecord,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(owner_record)?;
    std::fs::write(owner_record_path(workspace_path)?, bytes)?;
    Ok(())
}

pub(super) async fn remove_worktree(
    source_repo: &Path,
    workspace_path: &Path,
) -> anyhow::Result<()> {
    let output = git_command()
        .args([
            "-C",
            &source_repo.to_string_lossy(),
            "worktree",
            "remove",
            "--force",
            &workspace_path.to_string_lossy(),
        ])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if workspace_path.exists() {
            tracing::warn!(
                "orphan workspace {:?} is not a git worktree — delete it manually: rm -rf {:?}",
                workspace_path,
                workspace_path
            );
        }
        anyhow::bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
}

pub(super) async fn cleanup_workspace_path(
    source_repo: &Path,
    workspace_path: &Path,
) -> anyhow::Result<()> {
    cleanup_workspace_path_with_registration(source_repo, workspace_path, None).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceReclaimOutcome {
    Deleted,
    SkippedLiveLease {
        task_id: TaskId,
        owner_session: String,
    },
    SkippedLeaseLookupFailed {
        error: String,
    },
}

pub(super) async fn try_reclaim_workspace(
    source_repo: &Path,
    workspace_path: &Path,
    lease_store: Option<&WorkspaceLeaseStore>,
    known_worktree_registered: Option<bool>,
) -> anyhow::Result<WorkspaceReclaimOutcome> {
    if let Some(store) = lease_store {
        match store.leased_workspace_path(workspace_path).await {
            Ok(Some(record)) => {
                return Ok(WorkspaceReclaimOutcome::SkippedLiveLease {
                    task_id: record.task_id,
                    owner_session: record.owner_session,
                });
            }
            Ok(None) => {}
            Err(error) => {
                return Ok(WorkspaceReclaimOutcome::SkippedLeaseLookupFailed {
                    error: error.to_string(),
                });
            }
        }
    }

    cleanup_workspace_path_with_registration(
        source_repo,
        workspace_path,
        known_worktree_registered,
    )
    .await?;
    Ok(WorkspaceReclaimOutcome::Deleted)
}

pub(super) async fn cleanup_workspace_path_with_registration(
    source_repo: &Path,
    workspace_path: &Path,
    known_worktree_registered: Option<bool>,
) -> anyhow::Result<()> {
    let worktree_registered = match known_worktree_registered {
        Some(registered) => registered,
        None => is_registered_worktree(source_repo, workspace_path).await,
    };
    if worktree_registered {
        match remove_worktree(source_repo, workspace_path).await {
            Ok(()) => {}
            Err(e) if !workspace_path.exists() => {
                tracing::warn!(
                    path = ?workspace_path,
                    "cleanup_workspace_path: git worktree remove failed for missing path; pruning stale metadata: {e}"
                );
            }
            Err(e) => {
                tracing::warn!(path = ?workspace_path, "cleanup_workspace_path: git worktree remove failed for existing path: {e}");
            }
        }
    }

    if workspace_path.exists() {
        std::fs::remove_dir_all(workspace_path)?;
    }

    if let Err(e) = git_command()
        .args(["-C", &source_repo.to_string_lossy(), "worktree", "prune"])
        .output()
        .await
    {
        tracing::warn!("cleanup_workspace_path: git worktree prune failed: {e}");
    }

    Ok(())
}

pub(super) async fn resolve_cleanup_source_repo(
    default_source_repo: &Path,
    workspace_path: &Path,
    task: Option<&TaskSummary>,
) -> PathBuf {
    if let Some(project_root) = task.and_then(|task| task.project.as_deref()) {
        return PathBuf::from(project_root);
    }

    infer_workspace_source_repo(workspace_path)
        .await
        .unwrap_or_else(|| default_source_repo.to_path_buf())
}

pub(super) async fn infer_workspace_source_repo(workspace_path: &Path) -> Option<PathBuf> {
    git_command()
        .args([
            "-C",
            &workspace_path.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| PathBuf::from(stdout.trim()))
}

pub(super) async fn is_registered_worktree(source_repo: &Path, workspace_path: &Path) -> bool {
    // `git worktree list --porcelain` emits absolute paths even when `workspace.root`
    // was configured relatively. Deleted worktrees may still be listed through a
    // symlink-expanded parent such as `/private/var`, so normalize through the
    // nearest existing ancestor before matching.
    let expected_path = canonicalize_existing_or_parent(workspace_path);
    git_command()
        .args([
            "-C",
            &source_repo.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| {
            stdout.lines().any(|line| {
                line.strip_prefix("worktree ")
                    .map(PathBuf::from)
                    .map(|listed| canonicalize_existing_or_parent(&listed))
                    .is_some_and(|listed| listed == expected_path)
            })
        })
        .unwrap_or(false)
}

pub(super) async fn reset_registered_worktree(
    workspace_path: &Path,
    branch: &str,
    target_ref: &str,
) -> anyhow::Result<()> {
    let reset_pre = git_command()
        .args(["-C", &workspace_path.to_string_lossy(), "reset", "--hard"])
        .output()
        .await?;
    if !reset_pre.status.success() {
        anyhow::bail!(
            "git reset --hard failed before checkout: {}",
            String::from_utf8_lossy(&reset_pre.stderr).trim()
        );
    }

    let clean_pre = git_command()
        .args(["-C", &workspace_path.to_string_lossy(), "clean", "-fdx"])
        .output()
        .await?;
    if !clean_pre.status.success() {
        anyhow::bail!(
            "git clean -fdx failed before checkout: {}",
            String::from_utf8_lossy(&clean_pre.stderr).trim()
        );
    }

    let checkout = git_command()
        .args([
            "-C",
            &workspace_path.to_string_lossy(),
            "checkout",
            "-B",
            branch,
            target_ref,
        ])
        .output()
        .await?;
    if !checkout.status.success() {
        anyhow::bail!(
            "git checkout -B failed: {}",
            String::from_utf8_lossy(&checkout.stderr).trim()
        );
    }

    let reset = git_command()
        .args([
            "-C",
            &workspace_path.to_string_lossy(),
            "reset",
            "--hard",
            target_ref,
        ])
        .output()
        .await?;
    if !reset.status.success() {
        anyhow::bail!(
            "git reset --hard failed: {}",
            String::from_utf8_lossy(&reset.stderr).trim()
        );
    }

    let clean = git_command()
        .args(["-C", &workspace_path.to_string_lossy(), "clean", "-fdx"])
        .output()
        .await?;
    if !clean.status.success() {
        anyhow::bail!(
            "git clean -fdx failed: {}",
            String::from_utf8_lossy(&clean.stderr).trim()
        );
    }

    Ok(())
}

pub(super) fn slot_index_from_workspace_path(
    project_key: &str,
    workspace_path: &Path,
) -> Option<u32> {
    let name = workspace_path.file_name()?.to_str()?;
    let prefix = format!("{project_key}__slot_");
    name.strip_prefix(&prefix)?.parse().ok()
}

pub(super) fn canonicalize_existing_or_parent(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut missing_components = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        let Some(file_name) = cursor.file_name() else {
            return path.to_path_buf();
        };
        missing_components.push(file_name.to_os_string());
        cursor = parent;
    }

    let mut normalized = std::fs::canonicalize(cursor).unwrap_or_else(|_| cursor.to_path_buf());
    for component in missing_components.iter().rev() {
        normalized.push(component);
    }
    normalized
}
