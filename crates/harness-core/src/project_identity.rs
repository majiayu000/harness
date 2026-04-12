use std::path::{Path, PathBuf};

/// Canonical project identity used for execution, queueing, and config lookup.
///
/// `canonical_root` is the single internal identity key. `alias_id` preserves a
/// human-facing registry/project ID when the caller submitted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    canonical_root: PathBuf,
    alias_id: Option<String>,
}

impl ProjectIdentity {
    pub fn new(canonical_root: PathBuf, alias_id: Option<String>) -> Self {
        Self {
            canonical_root,
            alias_id,
        }
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self::new(canonicalize_root(root), None)
    }

    pub fn from_registry(alias_id: String, root: PathBuf) -> Self {
        Self::new(canonicalize_root(root), Some(alias_id))
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn into_canonical_root(self) -> PathBuf {
        self.canonical_root
    }

    pub fn alias_id(&self) -> Option<&str> {
        self.alias_id.as_deref()
    }

    pub fn queue_key(&self) -> String {
        self.canonical_root.display().to_string()
    }
}

pub fn canonicalize_root(root: PathBuf) -> PathBuf {
    root.canonicalize().unwrap_or(root)
}

pub fn canonical_project_key(project: impl AsRef<Path>) -> String {
    canonicalize_root(project.as_ref().to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_project_key_preserves_nonexistent_path() {
        let path = PathBuf::from("/path/that/does/not/exist");
        assert_eq!(canonical_project_key(&path), path.display().to_string());
    }

    #[test]
    fn project_identity_from_registry_preserves_alias() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root)?;

        let identity = ProjectIdentity::from_registry("repo-id".to_string(), root.canonicalize()?);
        assert_eq!(identity.alias_id(), Some("repo-id"));
        assert_eq!(
            identity.queue_key(),
            root.canonicalize()?.display().to_string()
        );
        Ok(())
    }
}
