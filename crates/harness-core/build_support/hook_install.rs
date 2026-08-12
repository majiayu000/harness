use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const GIT_DIR_PREFIX: &str = "gitdir: ";

pub fn install_pre_commit_hook(workspace_root: &Path) -> io::Result<Option<PathBuf>> {
    let source = workspace_root.join(".githooks").join("pre-commit");
    if !source.is_file() {
        return Ok(None);
    }

    let Some(common_git_dir) = resolve_common_git_dir(workspace_root)? else {
        return Ok(None);
    };
    let hooks_dir = common_git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir)?;

    let destination = hooks_dir.join("pre-commit");
    if fs::read(&destination).ok().as_deref() != Some(fs::read(&source)?.as_slice()) {
        fs::copy(&source, &destination)?;
    }
    make_executable(&destination)?;
    Ok(Some(destination))
}

pub fn resolve_common_git_dir(workspace_root: &Path) -> io::Result<Option<PathBuf>> {
    let dot_git = workspace_root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    if !dot_git.is_file() {
        return Ok(None);
    }

    let pointer = fs::read_to_string(&dot_git)?;
    let git_dir = pointer
        .trim()
        .strip_prefix(GIT_DIR_PREFIX)
        .filter(|path| !path.trim().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid .git pointer"))?;
    let git_dir = resolve_path(workspace_root, Path::new(git_dir))?;
    let common_dir_file = git_dir.join("commondir");
    if !common_dir_file.is_file() {
        return Ok(Some(git_dir));
    }

    let common_dir = fs::read_to_string(common_dir_file)?;
    let common_dir = common_dir.trim();
    if common_dir.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty git commondir pointer",
        ));
    }
    resolve_path(&git_dir, Path::new(common_dir)).map(Some)
}

fn resolve_path(base: &Path, path: &Path) -> io::Result<PathBuf> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    candidate.canonicalize()
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}
