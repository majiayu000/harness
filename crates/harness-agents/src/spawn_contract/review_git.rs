use harness_core::error::HarnessError;
use std::path::{Path, PathBuf};

use super::CONTAINER_WORKSPACE;

pub(super) struct ContainerMount {
    pub(super) source: PathBuf,
    pub(super) destination: PathBuf,
}

pub(super) struct ReviewGitLayout {
    pub(super) workspace_source: PathBuf,
    pub(super) child_workspace: PathBuf,
    pub(super) git_mounts: Vec<ContainerMount>,
    pub(super) git_env: Vec<(&'static str, PathBuf)>,
}

pub(super) fn plan(project_root: &Path) -> Result<ReviewGitLayout, HarnessError> {
    let (repository_root, dot_git) = find_repository_root(project_root)?;
    let relative_project = project_root.strip_prefix(&repository_root).map_err(|_| {
        unsupported(
            "topology_invalid",
            format!(
                "project root {} is outside repository root {}",
                project_root.display(),
                repository_root.display()
            ),
        )
    })?;
    let child_workspace = if relative_project.as_os_str().is_empty() {
        PathBuf::from(CONTAINER_WORKSPACE)
    } else {
        Path::new(CONTAINER_WORKSPACE).join(relative_project)
    };
    let metadata = std::fs::symlink_metadata(&dot_git).map_err(|error| {
        unsupported(
            "topology_unreadable",
            format!("{}: {error}", dot_git.display()),
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(unsupported(
            "topology_unsupported",
            format!("symbolic .git path {}", dot_git.display()),
        ));
    }
    if metadata.is_dir() {
        return Ok(ReviewGitLayout {
            workspace_source: repository_root,
            child_workspace,
            git_mounts: Vec::new(),
            git_env: Vec::new(),
        });
    }
    if !metadata.is_file() {
        return Err(unsupported(
            "topology_unsupported",
            format!(
                ".git is neither a file nor directory at {}",
                dot_git.display()
            ),
        ));
    }

    let git_dir = canonical_git_pointer(&dot_git)?;
    let common_dir = canonical_common_git_dir(&git_dir)?;
    let mut git_mounts = vec![ContainerMount {
        source: git_dir.clone(),
        destination: PathBuf::from("/git/worktree"),
    }];
    let common_destination = if common_dir == git_dir {
        PathBuf::from("/git/worktree")
    } else {
        git_mounts.push(ContainerMount {
            source: common_dir,
            destination: PathBuf::from("/git/common"),
        });
        PathBuf::from("/git/common")
    };
    Ok(ReviewGitLayout {
        workspace_source: repository_root,
        child_workspace,
        git_mounts,
        git_env: vec![
            ("GIT_DIR", PathBuf::from("/git/worktree")),
            ("GIT_COMMON_DIR", common_destination),
            ("GIT_WORK_TREE", PathBuf::from(CONTAINER_WORKSPACE)),
        ],
    })
}

fn find_repository_root(project_root: &Path) -> Result<(PathBuf, PathBuf), HarnessError> {
    for ancestor in project_root.ancestors() {
        let dot_git = ancestor.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Ok(_) => return Ok((ancestor.to_path_buf(), dot_git)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(unsupported(
                    "topology_unreadable",
                    format!("{}: {error}", dot_git.display()),
                ));
            }
        }
    }
    Err(unsupported(
        "repository_not_found",
        format!("no .git ancestor for {}", project_root.display()),
    ))
}

fn canonical_git_pointer(dot_git: &Path) -> Result<PathBuf, HarnessError> {
    let contents = std::fs::read_to_string(dot_git).map_err(|error| {
        unsupported(
            "pointer_unreadable",
            format!("{}: {error}", dot_git.display()),
        )
    })?;
    let target = parse_single_path_record(&contents, "gitdir:")
        .ok_or_else(|| unsupported("pointer_invalid", dot_git.display().to_string()))?;
    let parent = dot_git.parent().ok_or_else(|| {
        unsupported(
            "pointer_invalid",
            format!("{} has no parent", dot_git.display()),
        )
    })?;
    canonical_git_path(parent, target)
}

fn canonical_common_git_dir(git_dir: &Path) -> Result<PathBuf, HarnessError> {
    let commondir = git_dir.join("commondir");
    let contents = match std::fs::read_to_string(&commondir) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(git_dir.to_path_buf());
        }
        Err(error) => {
            return Err(unsupported(
                "commondir_unreadable",
                format!("{}: {error}", commondir.display()),
            ));
        }
    };
    let target = parse_single_path_record(&contents, "")
        .ok_or_else(|| unsupported("commondir_invalid", commondir.display().to_string()))?;
    canonical_git_path(git_dir, target)
}

fn parse_single_path_record<'a>(contents: &'a str, prefix: &str) -> Option<&'a str> {
    let mut nonempty = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let line = nonempty.next()?;
    if nonempty.next().is_some() {
        return None;
    }
    line.strip_prefix(prefix)
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn canonical_git_path(base: &Path, target: &str) -> Result<PathBuf, HarnessError> {
    let target = Path::new(target);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    };
    let canonical = std::fs::canonicalize(&target).map_err(|error| {
        unsupported("path_unavailable", format!("{}: {error}", target.display()))
    })?;
    if !canonical.is_dir() {
        return Err(unsupported(
            "path_not_directory",
            canonical.display().to_string(),
        ));
    }
    Ok(canonical)
}

fn unsupported(code: &str, evidence: String) -> HarnessError {
    HarnessError::Unsupported(format!("container_review_git_{code}: {evidence}"))
}
