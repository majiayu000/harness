//! ProjectService — registry CRUD, path resolution, and default root lookup.

use crate::project_registry::{Project, ProjectRegistry};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Trait interface for project registry operations.
///
/// Implementations may use SQLite persistence ([`DefaultProjectService`]) or
/// an in-memory store for tests.
#[async_trait]
pub trait ProjectService: Send + Sync {
    /// Register or update a project in the registry.
    async fn register(&self, project: Project) -> anyhow::Result<()>;

    /// Retrieve a project by its registered ID.
    async fn get(&self, id: &str) -> anyhow::Result<Option<Project>>;

    /// List all registered projects.
    async fn list(&self) -> anyhow::Result<Vec<Project>>;

    /// Remove a project by ID. Returns `true` if it existed.
    async fn remove(&self, id: &str) -> anyhow::Result<bool>;

    /// Resolve a registered project ID to its root path.
    /// Returns `None` when the ID is not found.
    async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>>;

    /// The default project root configured at server startup.
    fn default_root(&self) -> &Path;
}

/// Production implementation backed by [`ProjectRegistry`] (SQLite) and a
/// configured default project root.
pub struct DefaultProjectService {
    registry: Arc<ProjectRegistry>,
    root: PathBuf,
}

impl DefaultProjectService {
    pub fn new(registry: Arc<ProjectRegistry>, root: PathBuf) -> Arc<Self> {
        Arc::new(Self { registry, root })
    }
}

#[async_trait]
impl ProjectService for DefaultProjectService {
    async fn register(&self, project: Project) -> anyhow::Result<()> {
        self.registry.register(project).await
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
        self.registry.get(id).await
    }

    async fn list(&self) -> anyhow::Result<Vec<Project>> {
        self.registry.list().await
    }

    async fn remove(&self, id: &str) -> anyhow::Result<bool> {
        self.registry.remove(id).await
    }

    async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        self.registry.resolve_path(id).await
    }

    fn default_root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    /// In-memory mock for unit tests that do not need SQLite.
    struct MockProjectService {
        store: RwLock<HashMap<String, Project>>,
        root: PathBuf,
    }

    impl MockProjectService {
        fn new(root: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                store: RwLock::new(HashMap::new()),
                root,
            })
        }
    }

    #[async_trait]
    impl ProjectService for MockProjectService {
        async fn register(&self, project: Project) -> anyhow::Result<()> {
            self.store.write().await.insert(project.id.clone(), project);
            Ok(())
        }

        async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
            Ok(self.store.read().await.get(id).cloned())
        }

        async fn list(&self) -> anyhow::Result<Vec<Project>> {
            Ok(self.store.read().await.values().cloned().collect())
        }

        async fn remove(&self, id: &str) -> anyhow::Result<bool> {
            Ok(self.store.write().await.remove(id).is_some())
        }

        async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
            Ok(self.store.read().await.get(id).map(|p| p.root.clone()))
        }

        fn default_root(&self) -> &Path {
            &self.root
        }
    }

    fn make_project(id: &str, root: &str) -> Project {
        Project {
            id: id.to_string(),
            root: PathBuf::from(root),
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn default_project_service_register_and_get() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = DefaultProjectService::new(registry, dir.path().to_path_buf());

        svc.register(make_project("alpha", "/tmp/alpha")).await?;

        let loaded = svc.get("alpha").await?.expect("should exist");
        assert_eq!(loaded.id, "alpha");
        assert_eq!(loaded.root, PathBuf::from("/tmp/alpha"));
        Ok(())
    }

    #[tokio::test]
    async fn default_project_service_resolve_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = DefaultProjectService::new(registry, dir.path().to_path_buf());

        svc.register(make_project("beta", "/home/user/beta"))
            .await?;

        assert_eq!(
            svc.resolve_path("beta").await?,
            Some(PathBuf::from("/home/user/beta"))
        );
        assert_eq!(svc.resolve_path("missing").await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn default_project_service_remove() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = DefaultProjectService::new(registry, dir.path().to_path_buf());

        svc.register(make_project("gamma", "/tmp/gamma")).await?;
        assert!(svc.remove("gamma").await?);
        assert!(!svc.remove("gamma").await?);
        assert!(svc.get("gamma").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn default_project_service_default_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = DefaultProjectService::new(registry, root.clone());
        assert_eq!(svc.default_root(), root.as_path());
        Ok(())
    }

    #[tokio::test]
    async fn mock_project_service_roundtrip() -> anyhow::Result<()> {
        let svc = MockProjectService::new(PathBuf::from("/project"));

        svc.register(make_project("x", "/tmp/x")).await?;
        assert_eq!(svc.resolve_path("x").await?, Some(PathBuf::from("/tmp/x")));
        assert_eq!(svc.list().await?.len(), 1);
        assert!(svc.remove("x").await?);
        assert_eq!(svc.list().await?.len(), 0);
        Ok(())
    }
}
