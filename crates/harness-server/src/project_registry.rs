use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const PROJECT_REGISTRY_SCHEMA: &str = "project_registry";

/// A registered project with its root path and optional config overrides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub root: PathBuf,
    /// Human-readable name from `[[projects]] name = "..."` config; used as
    /// a secondary lookup key so runtime submissions with
    /// `{"project":"litellm"}` resolve even though the primary id is the
    /// canonical filesystem path.
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

static PROJECT_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create projects table",
        sql: "CREATE TABLE IF NOT EXISTS projects (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "index project name lookups",
        sql: "CREATE INDEX IF NOT EXISTS idx_projects_name_created_at
              ON projects ((data::jsonb ->> 'name'), created_at DESC)",
    },
    Migration {
        version: 3,
        description: "index project root lookups",
        sql: "CREATE INDEX IF NOT EXISTS idx_projects_root_created_at
              ON projects ((data::jsonb ->> 'root'), created_at DESC)",
    },
    Migration {
        version: 4,
        description: "scope projects within shared schema",
        sql: "ALTER TABLE projects ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE projects
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE projects ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE projects ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE projects DROP CONSTRAINT IF EXISTS projects_pkey;
              ALTER TABLE projects ADD CONSTRAINT projects_pkey PRIMARY KEY (store_key, id);
              DROP INDEX IF EXISTS idx_projects_name_created_at;
              DROP INDEX IF EXISTS idx_projects_root_created_at;
              CREATE INDEX IF NOT EXISTS idx_projects_store_created_at
              ON projects (store_key, created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_projects_store_name_created_at
              ON projects (store_key, (data::jsonb ->> 'name'), created_at DESC);
              CREATE INDEX IF NOT EXISTS idx_projects_store_root_created_at
              ON projects (store_key, (data::jsonb ->> 'root'), created_at DESC)",
    },
    Migration {
        version: 5,
        description: "record legacy project registry backfills",
        sql: "CREATE TABLE IF NOT EXISTS project_registry_legacy_backfills (
            store_key     TEXT NOT NULL DEFAULT current_schema(),
            legacy_schema TEXT NOT NULL,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (store_key, legacy_schema)
        )",
    },
];

/// Registry of projects backed by Postgres. Survives server restarts.
pub struct ProjectRegistry {
    pool: PgPool,
    schema: String,
    store_key: String,
}

impl ProjectRegistry {
    pub async fn open(path: &Path) -> anyhow::Result<Arc<Self>> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let context = PgStoreContext::from_legacy_path_schema(path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let store_key = schema.clone();
        let pool = context.open_migrated_pool(PROJECT_MIGRATIONS).await?;
        Ok(Arc::new(Self {
            pool,
            schema,
            store_key,
        }))
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PostgresBackend::new(configured_database_url.map(ToOwned::to_owned)).store_context(
            &StoreLocation::SharedSchema(PROJECT_REGISTRY_SCHEMA.to_string()),
        )
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open_with_context_and_store_key(context, setup_pool, context.schema().to_owned())
            .await
    }

    pub async fn open_shared_with_data_dir(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        data_dir: &Path,
    ) -> anyhow::Result<Arc<Self>> {
        let store_key = Self::store_key_for_data_dir(data_dir);
        Self::open_with_context_and_store_key(context, setup_pool, store_key).await
    }

    async fn open_with_context_and_store_key(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Arc<Self>> {
        let schema = context.schema().to_owned();
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, PROJECT_MIGRATIONS)
            .await?;
        Ok(Arc::new(Self {
            pool,
            schema,
            store_key,
        }))
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> String {
        let path = data_dir.canonicalize().unwrap_or_else(|_| {
            if data_dir.is_absolute() {
                data_dir.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|current| current.join(data_dir))
                    .unwrap_or_else(|_| data_dir.to_path_buf())
            }
        });
        path.to_string_lossy().into_owned()
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    /// Register or update a project.
    pub async fn register(&self, project: Project) -> anyhow::Result<()> {
        let data = serde_json::to_string(&project)?;
        sqlx::query(
            "INSERT INTO projects (store_key, id, data) VALUES ($1, $2, $3)
             ON CONFLICT(store_key, id) DO UPDATE SET data = EXCLUDED.data,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&self.store_key)
        .bind(&project.id)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all registered projects ordered by creation time (newest first).
    pub async fn list(&self) -> anyhow::Result<Vec<Project>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data FROM projects WHERE store_key = $1 ORDER BY created_at DESC",
        )
        .bind(&self.store_key)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Get a project by ID.
    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Project>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data FROM projects WHERE store_key = $1 AND id = $2")
                .bind(&self.store_key)
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
        let result = sqlx::query("DELETE FROM projects WHERE store_key = $1 AND id = $2")
            .bind(&self.store_key)
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
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data
             FROM projects
             WHERE store_key = $1 AND data::jsonb ->> 'name' = $2
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    /// Find a project by its canonical root path. Returns the newest match.
    pub async fn get_by_root(&self, root: &std::path::Path) -> anyhow::Result<Option<Project>> {
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root_str = canonical_root.to_string_lossy();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data
             FROM projects
             WHERE store_key = $1 AND data::jsonb ->> 'root' = $2
             ORDER BY created_at DESC
             LIMIT 1",
        )
        .bind(&self.store_key)
        .bind(root_str.as_ref())
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }
}

pub async fn migrate_legacy_project_registry_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_registry: &ProjectRegistry,
) -> anyhow::Result<u64> {
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_registry.schema() {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM project_registry_legacy_backfills
         WHERE store_key = $1 AND legacy_schema = $2",
    )
    .bind(target_registry.store_key())
    .bind(legacy_schema)
    .fetch_optional(&target_registry.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_table: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("\"{legacy_schema}\".projects"))
        .fetch_one(&target_registry.pool)
        .await?;
    if legacy_table.is_none() {
        return Ok(0);
    }

    let mut tx = target_registry.pool.begin().await?;
    let copy_sql = format!(
        "INSERT INTO projects (store_key, id, data, created_at, updated_at)
         SELECT $1, id, data, created_at, updated_at
         FROM \"{legacy_schema}\".projects
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    let copied = sqlx::query(&copy_sql)
        .bind(target_registry.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    sqlx::query(
        "INSERT INTO project_registry_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_registry.store_key())
    .bind(legacy_schema)
    .bind(copied as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied > 0 {
        tracing::info!(
            copied,
            legacy_schema,
            target_schema = target_registry.schema(),
            "project registry migration: backfilled legacy projects into shared schema"
        );
    }

    Ok(copied)
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
#[path = "project_registry_tests.rs"]
mod tests;
