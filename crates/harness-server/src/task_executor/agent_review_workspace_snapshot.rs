use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::fs;
use std::hash::Hasher;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct WorkspaceSnapshot {
    files: BTreeMap<PathBuf, EntryFingerprint>,
}

#[derive(Debug, PartialEq, Eq)]
enum EntryFingerprint {
    File(FileFingerprint),
    Symlink { target: PathBuf },
}

#[derive(Debug, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    hash: u64,
    mode: u32,
}

impl WorkspaceSnapshot {
    pub(super) fn capture(root: &Path) -> Result<Self, String> {
        let mut files = BTreeMap::new();
        collect_snapshot_files(root, root, &mut files)?;
        Ok(Self { files })
    }

    pub(super) fn changed_since(&self, root: &Path) -> Result<bool, String> {
        Ok(*self != Self::capture(root)?)
    }
}

fn collect_snapshot_files(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<PathBuf, EntryFingerprint>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|error| {
        format!(
            "failed to read workspace directory {}: {error}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read workspace entry in {}: {error}",
                dir.display()
            )
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            format!("failed to stat workspace entry {}: {error}", path.display())
        })?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if file_type.is_dir() {
            if should_skip_snapshot_dir(&name) {
                continue;
            }
            collect_snapshot_files(root, &path, files)?;
            continue;
        }
        if should_skip_snapshot_file(&name) {
            continue;
        }
        let relative = relative_snapshot_path(root, &path)?;
        if file_type.is_symlink() {
            files.insert(relative, fingerprint_symlink(&path)?);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        files.insert(relative, fingerprint_file(&path)?);
    }
    Ok(())
}

fn should_skip_snapshot_dir(name: &str) -> bool {
    const SKIPPED_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        ".next",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".tox",
        ".nox",
        ".cache",
        ".parcel-cache",
        ".turbo",
        ".svelte-kit",
        ".vite",
        ".gradle",
        "coverage",
        "__pycache__",
        ".venv",
        "venv",
    ];
    SKIPPED_DIRS.contains(&name)
}

fn should_skip_snapshot_file(name: &str) -> bool {
    name == ".DS_Store"
        || name == "tasks.db"
        || name.starts_with("tasks.db-")
        || name == "events.db"
        || name.starts_with("events.db-")
}

fn relative_snapshot_path(root: &Path, path: &Path) -> Result<PathBuf, String> {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|error| format!("failed to normalize {}: {error}", path.display()))
}

fn fingerprint_file(path: &Path) -> Result<EntryFingerprint, String> {
    let mode = fingerprint_mode(path)?;
    let mut file = fs::File::open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("failed to stat {}: {error}", path.display()))?;
    let mut hasher = DefaultHasher::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        hasher.write(&buffer[..bytes_read]);
    }
    Ok(EntryFingerprint::File(FileFingerprint {
        len: metadata.len(),
        hash: hasher.finish(),
        mode,
    }))
}

fn fingerprint_symlink(path: &Path) -> Result<EntryFingerprint, String> {
    fs::read_link(path)
        .map(|target| EntryFingerprint::Symlink { target })
        .map_err(|error| format!("failed to read symlink {}: {error}", path.display()))
}

#[cfg(unix)]
fn fingerprint_mode(path: &Path) -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt;

    fs::metadata(path)
        .map(|metadata| metadata.mode())
        .map_err(|error| format!("failed to stat {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn fingerprint_mode(path: &Path) -> Result<u32, String> {
    fs::metadata(path)
        .map(|metadata| u32::from(metadata.permissions().readonly()))
        .map_err(|error| format!("failed to stat {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::WorkspaceSnapshot;

    #[test]
    fn changed_since_detects_tracked_output_directory_changes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let dist_file = dir.path().join("dist").join("bundle.js");
        let build_file = dir.path().join("build").join("artifact.txt");
        std::fs::create_dir_all(dist_file.parent().expect("dist parent"))?;
        std::fs::create_dir_all(build_file.parent().expect("build parent"))?;
        std::fs::write(&dist_file, "old")?;
        std::fs::write(&build_file, "old")?;

        let snapshot = WorkspaceSnapshot::capture(dir.path()).map_err(anyhow::Error::msg)?;
        std::fs::write(&dist_file, "new")?;
        assert!(snapshot
            .changed_since(dir.path())
            .map_err(anyhow::Error::msg)?);

        let snapshot = WorkspaceSnapshot::capture(dir.path()).map_err(anyhow::Error::msg)?;
        std::fs::write(&build_file, "new")?;
        assert!(snapshot
            .changed_since(dir.path())
            .map_err(anyhow::Error::msg)?);

        Ok(())
    }

    #[test]
    fn changed_since_ignores_large_cache_directories() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_files = [
            dir.path().join("target").join("debug").join("artifact"),
            dir.path().join(".pytest_cache").join("nodeids"),
            dir.path().join(".mypy_cache").join("module.meta.json"),
        ];
        for cache_file in &cache_files {
            std::fs::create_dir_all(cache_file.parent().expect("cache parent"))?;
            std::fs::write(cache_file, "old")?;
        }

        let snapshot = WorkspaceSnapshot::capture(dir.path()).map_err(anyhow::Error::msg)?;
        for cache_file in &cache_files {
            std::fs::write(cache_file, "new")?;
        }
        assert!(!snapshot
            .changed_since(dir.path())
            .map_err(anyhow::Error::msg)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn changed_since_detects_executable_bit_changes() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let script = dir.path().join("script.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n")?;

        let mut permissions = std::fs::metadata(&script)?.permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&script, permissions)?;

        let snapshot = WorkspaceSnapshot::capture(dir.path()).map_err(anyhow::Error::msg)?;

        let mut permissions = std::fs::metadata(&script)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions)?;

        assert!(snapshot
            .changed_since(dir.path())
            .map_err(anyhow::Error::msg)?);

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn changed_since_detects_symlink_target_changes() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("target-a.txt"), "a")?;
        std::fs::write(dir.path().join("target-b.txt"), "b")?;
        let link = dir.path().join("current.txt");
        symlink("target-a.txt", &link)?;

        let snapshot = WorkspaceSnapshot::capture(dir.path()).map_err(anyhow::Error::msg)?;

        std::fs::remove_file(&link)?;
        symlink("target-b.txt", &link)?;

        assert!(snapshot
            .changed_since(dir.path())
            .map_err(anyhow::Error::msg)?);

        Ok(())
    }
}
