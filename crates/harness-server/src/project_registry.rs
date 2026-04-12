use harness_core::db::{Db, DbEntity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// A registered project with its root path and optional config overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub root: PathBuf,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    #[serde(default)]
    pub default_agent: Option<String>,
    #[serde(default = "default_active")]
    pub active: bool,
    pub created_at: String,
}

fn default_active() -> bool {
    true
}

impl DbEntity for Project {
    fn table_name() -> &'static str {
        "projects"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn create_table_sql() -> &'static str {
        "CREATE TABLE IF NOT EXISTS projects (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"
    }
}

/// Registry of projects backed by SQLite. Survives server restarts.
pub struct ProjectRegistry {
    db: Db<Project>,
}

impl ProjectRegistry {
    pub async fn open(path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let db = Db::open(path).await?;
        Ok(Arc::new(Self { db }))
    }

    /// Register or update a project.
    pub async fn register(&self, project: Project) -> anyhow::Result<()> {
        self.db.upsert(&project).await
    }

    /// List all registered projects ordered by creation time (newest first).
    pub async fn list(&self) -> anyhow::Result<Vec<Project>> {
        self.db.list().await
    }

    /// Get a project by ID.
    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
        self.db.get(id).await
    }

    /// Remove a project by ID. Returns `true` if it existed.
    pub async fn remove(&self, id: &str) -> anyhow::Result<bool> {
        self.db.delete(id).await
    }

    /// Resolve a project ID to its root path. Returns `None` if not found.
    pub async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        Ok(self.get(id).await?.map(|p| p.root))
    }
}

/// Check that `canonical_root` falls under at least one of the
/// `allowed_project_roots`.  Each allowlist entry is canonicalized before the
/// prefix check so that relative paths and symlinks in the config don't cause
/// false 403s.  Returns `Ok(())` when the allowlist is empty (i.e. no
/// restriction configured).
pub fn check_allowed_roots(
    canonical_root: &std::path::Path,
    allowed: &[std::path::PathBuf],
) -> Result<(), String> {
    if allowed.is_empty() {
        return Ok(());
    }
    let matched = allowed.iter().any(|base| {
        base.canonicalize()
            .map(|canon_base| canonical_root.starts_with(&canon_base))
            .unwrap_or(false)
    });
    if !matched {
        return Err("project root is not under an allowed base directory".to_string());
    }
    Ok(())
}

/// Validate that a path is an existing directory and a git repository.
pub fn validate_project_root(root: &std::path::Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()));
    }
    // Check for .git directory or .git file (worktree case)
    if !root.join(".git").exists() {
        return Err(format!(
            "root is not a git repository (no .git found): {}",
            root.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_get_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;

        let project = Project {
            id: "my-project".to_string(),
            root: PathBuf::from("/tmp/my-project"),
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        registry.register(project.clone()).await?;

        let loaded = registry
            .get("my-project")
            .await?
            .expect("project should exist");
        assert_eq!(loaded.id, "my-project");
        assert_eq!(loaded.root, PathBuf::from("/tmp/my-project"));
        assert!(loaded.active);
        Ok(())
    }

    #[tokio::test]
    async fn list_returns_all_projects() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;

        for i in 0..3u32 {
            registry
                .register(Project {
                    id: format!("p{i}"),
                    root: PathBuf::from(format!("/tmp/p{i}")),
                    max_concurrent: None,
                    default_agent: None,
                    active: true,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                })
                .await?;
        }

        let all = registry.list().await?;
        assert_eq!(all.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn remove_returns_true_when_found() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;

        registry
            .register(Project {
                id: "to-delete".to_string(),
                root: PathBuf::from("/tmp/x"),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;

        assert!(registry.remove("to-delete").await?);
        assert!(registry.get("to-delete").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn remove_returns_false_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;
        assert!(!registry.remove("nonexistent").await?);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_path_returns_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join("projects.db")).await?;

        registry
            .register(Project {
                id: "harness".to_string(),
                root: PathBuf::from("/home/user/harness"),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;

        let path = registry.resolve_path("harness").await?;
        assert_eq!(path, Some(PathBuf::from("/home/user/harness")));
        assert!(registry.resolve_path("unknown").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn survives_reopen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("projects.db");

        {
            let registry = ProjectRegistry::open(&db_path).await?;
            registry
                .register(Project {
                    id: "persistent".to_string(),
                    root: PathBuf::from("/tmp/persistent"),
                    max_concurrent: Some(2),
                    default_agent: Some("claude".to_string()),
                    active: true,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                })
                .await?;
        }

        let registry = ProjectRegistry::open(&db_path).await?;
        let loaded = registry
            .get("persistent")
            .await?
            .expect("should survive reopen");
        assert_eq!(loaded.max_concurrent, Some(2));
        assert_eq!(loaded.default_agent.as_deref(), Some("claude"));
        Ok(())
    }

    #[tokio::test]
    async fn survives_reopen_with_partial_metadata() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("projects.db");

        {
            let registry = ProjectRegistry::open(&db_path).await?;
            registry
                .register(Project {
                    id: "partial".to_string(),
                    root: PathBuf::from("/tmp/partial"),
                    max_concurrent: None,
                    default_agent: None,
                    active: true,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                })
                .await?;
        }

        let registry = ProjectRegistry::open(&db_path).await?;
        let loaded = registry
            .get("partial")
            .await?
            .expect("should survive reopen");
        assert_eq!(loaded.max_concurrent, None);
        assert_eq!(loaded.default_agent, None);
        Ok(())
    }
}
