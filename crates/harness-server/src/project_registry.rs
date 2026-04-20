use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, Migration, PgMigrator,
};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

/// A registered project with its root path and optional config overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub root: PathBuf,
    /// Human-readable name from `[[projects]] name = "..."` config; used as
    /// a secondary lookup key so `POST /tasks {"project":"litellm"}` resolves
    /// even though the primary id is the canonical filesystem path.
    #[serde(default)]
    pub name: Option<String>,
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

static PROJECT_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "create projects table",
    sql: "CREATE TABLE IF NOT EXISTS projects (
        id         TEXT PRIMARY KEY,
        data       TEXT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
    )",
}];

/// Registry of projects backed by Postgres. Survives server restarts.
pub struct ProjectRegistry {
    pool: PgPool,
}

impl ProjectRegistry {
    pub async fn open(path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is not set"))?;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        let schema = format!("h{:016x}", hasher.finish());

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, PROJECT_MIGRATIONS).run().await?;
        Ok(Arc::new(Self { pool }))
    }

    /// Register or update a project.
    pub async fn register(&self, project: Project) -> anyhow::Result<()> {
        let data = serde_json::to_string(&project)?;
        sqlx::query(
            "INSERT INTO projects (id, data) VALUES ($1, $2)
             ON CONFLICT(id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&project.id)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all registered projects ordered by creation time (newest first).
    pub async fn list(&self) -> anyhow::Result<Vec<Project>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data FROM projects ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Get a project by ID.
    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT data FROM projects WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    /// Remove a project by ID. Returns `true` if it existed.
    pub async fn remove(&self, id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Resolve a project ID to its root path. Returns `None` if not found.
    pub async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        Ok(self.get(id).await?.map(|p| p.root))
    }

    /// Find a project by its configured `name` field. Returns the first match.
    pub async fn get_by_name(&self, name: &str) -> anyhow::Result<Option<Project>> {
        let projects = self.list().await?;
        Ok(projects
            .into_iter()
            .find(|p| p.name.as_deref() == Some(name)))
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

    async fn open_test_registry(name: &str) -> anyhow::Result<Option<Arc<ProjectRegistry>>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        let registry = ProjectRegistry::open(&dir.path().join(name)).await?;
        Ok(Some(registry))
    }

    #[tokio::test]
    async fn register_and_get_roundtrip() -> anyhow::Result<()> {
        let Some(registry) = open_test_registry("projects.db").await? else {
            return Ok(());
        };

        let project = Project {
            id: "my-project".to_string(),
            root: PathBuf::from("/tmp/my-project"),
            name: None,
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
        let Some(registry) = open_test_registry("projects.db").await? else {
            return Ok(());
        };

        for i in 0..3u32 {
            registry
                .register(Project {
                    id: format!("p{i}"),
                    root: PathBuf::from(format!("/tmp/p{i}")),
                    name: None,
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
        let Some(registry) = open_test_registry("projects.db").await? else {
            return Ok(());
        };

        registry
            .register(Project {
                id: "to-delete".to_string(),
                root: PathBuf::from("/tmp/x"),
                name: None,
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
        let Some(registry) = open_test_registry("projects.db").await? else {
            return Ok(());
        };
        assert!(!registry.remove("nonexistent").await?);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_path_returns_root() -> anyhow::Result<()> {
        let Some(registry) = open_test_registry("projects.db").await? else {
            return Ok(());
        };

        registry
            .register(Project {
                id: "harness".to_string(),
                root: PathBuf::from("/home/user/harness"),
                name: None,
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
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("projects.db");

        {
            let registry = ProjectRegistry::open(&db_path).await?;
            registry
                .register(Project {
                    id: "persistent".to_string(),
                    root: PathBuf::from("/tmp/persistent"),
                    name: None,
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
}
