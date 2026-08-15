use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const GIT_DIR_PREFIX: &str = "gitdir: ";
const MANAGED_HOOK: &[u8] = b"#!/bin/sh\n\
# harness-managed-pre-commit-v1\n\
set -eu\n\
repo_root=$(pwd -P)\n\
exec \"$repo_root/.githooks/pre-commit\" \"$@\"\n";

#[derive(Debug, Eq, PartialEq)]
pub enum InstallOutcome {
    Installed(PathBuf),
    AlreadyManaged(PathBuf),
    UnmanagedHookPreserved(PathBuf),
    MissingSource,
    NotRepository,
}

pub fn install_pre_commit_hook(workspace_root: &Path) -> io::Result<InstallOutcome> {
    let source = workspace_root.join(".githooks").join("pre-commit");
    if !source.is_file() {
        return Ok(InstallOutcome::MissingSource);
    }

    let Some(common_git_dir) = resolve_common_git_dir(workspace_root)? else {
        return Ok(InstallOutcome::NotRepository);
    };
    let hooks_dir = common_git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir)?;

    let destination = hooks_dir.join("pre-commit");
    match fs::read(&destination) {
        Ok(existing) if existing == MANAGED_HOOK => {
            make_executable(&destination)?;
            return Ok(InstallOutcome::AlreadyManaged(destination));
        }
        Ok(_) => return Ok(InstallOutcome::UnmanagedHookPreserved(destination)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    install_atomically(&hooks_dir, &destination)
}

fn install_atomically(hooks_dir: &Path, destination: &Path) -> io::Result<InstallOutcome> {
    let (temporary, mut file) = create_temporary_hook(hooks_dir)?;
    let result = (|| {
        file.write_all(MANAGED_HOOK)?;
        file.sync_all()?;
        make_executable(&temporary)?;

        match fs::hard_link(&temporary, destination) {
            Ok(()) => Ok(InstallOutcome::Installed(destination.to_path_buf())),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read(destination)? == MANAGED_HOOK {
                    make_executable(destination)?;
                    Ok(InstallOutcome::AlreadyManaged(destination.to_path_buf()))
                } else {
                    Ok(InstallOutcome::UnmanagedHookPreserved(
                        destination.to_path_buf(),
                    ))
                }
            }
            Err(error) => Err(error),
        }
    })();
    drop(file);
    let cleanup = fs::remove_file(temporary);
    match (result, cleanup) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(outcome), Ok(())) => Ok(outcome),
    }
}

fn create_temporary_hook(hooks_dir: &Path) -> io::Result<(PathBuf, fs::File)> {
    for attempt in 0..16 {
        let path = hooks_dir.join(format!(
            ".pre-commit.harness-{}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to reserve a temporary pre-commit hook path",
    ))
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
