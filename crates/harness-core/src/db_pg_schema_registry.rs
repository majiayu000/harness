use serde::Serialize;
use sqlx::postgres::PgPool;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

pub const PG_SCHEMA_REGISTRY_SCHEMA: &str = "harness_admin";
pub const PG_SCHEMA_REGISTRY_TABLE: &str = "schema_ownership";

const PATH_DERIVED_OWNER_KIND: &str = "path_derived_store";
const PATH_DERIVED_DIRECTORY_OWNER_KIND: &str = "path_derived_directory_store";
const PATH_DERIVED_RETENTION_CLASS: &str = "path_derived";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgSchemaOwnership {
    pub schema_name: String,
    pub owner_kind: String,
    pub owner_key: String,
    pub owner_path: Option<String>,
    pub retention_class: String,
}

impl PgSchemaOwnership {
    pub fn path_derived(schema_name: String, canonical_path: PathBuf) -> anyhow::Result<Self> {
        let owner_path = canonical_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", canonical_path))?
            .to_string();
        Ok(Self {
            schema_name,
            owner_kind: path_derived_owner_kind(&canonical_path).to_string(),
            owner_key: owner_path.clone(),
            owner_path: Some(owner_path),
            retention_class: PATH_DERIVED_RETENTION_CLASS.to_string(),
        })
    }
}

fn path_derived_owner_kind(path: &Path) -> &'static str {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => PATH_DERIVED_DIRECTORY_OWNER_KIND,
        _ => PATH_DERIVED_OWNER_KIND,
    }
}

fn is_path_derived_owner_kind(owner_kind: &str) -> bool {
    matches!(
        owner_kind,
        PATH_DERIVED_OWNER_KIND | PATH_DERIVED_DIRECTORY_OWNER_KIND
    )
}

fn normalize_path_for_comparison(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    absolute
        .canonicalize()
        .unwrap_or_else(|_| normalize_path_lexically(&absolute))
}

pub(crate) fn normalize_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::Prefix(_)) | Some(Component::RootDir) => {}
                Some(Component::ParentDir) | None => normalized.push(component.as_os_str()),
                Some(Component::CurDir) => unreachable!(),
            },
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn is_legacy_workspace_directory_owner_path(
    owner_path: &Path,
    workspace_roots: &[PathBuf],
) -> bool {
    if workspace_roots.is_empty() || !legacy_owner_path_has_workspace_key(owner_path) {
        return false;
    }
    let Some(parent) = owner_path.parent() else {
        return false;
    };
    let normalized_parent = normalize_path_lexically(parent);
    if workspace_roots.contains(&normalized_parent) {
        return true;
    }
    workspace_roots.contains(&normalize_path_for_comparison(parent))
}

fn legacy_owner_path_has_workspace_key(owner_path: &Path) -> bool {
    let Some(name) = owner_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    is_issue_or_pr_workspace_key(name)
        || is_uuid_or_suffixed_workspace_key(name)
        || name.starts_with("runtime-wf-")
        || name.starts_with("runtime-job-")
}

fn is_issue_or_pr_workspace_key(name: &str) -> bool {
    let Some(suffix) = name.rsplit("__").next() else {
        return false;
    };
    let number = if let Some(number) = suffix.strip_prefix("issue_") {
        number
    } else if let Some(number) = suffix.strip_prefix("pr_") {
        number
    } else {
        return false;
    };
    !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())
}

fn is_uuid_workspace_key(name: &str) -> bool {
    name.len() == 36
        && name.chars().enumerate().all(|(idx, ch)| match idx {
            8 | 13 | 18 | 23 => ch == '-',
            _ => ch.is_ascii_hexdigit(),
        })
}

fn is_uuid_or_suffixed_workspace_key(name: &str) -> bool {
    if is_uuid_workspace_key(name) {
        return true;
    }
    let Some((prefix, suffix)) = name.rsplit_once('-') else {
        return false;
    };
    is_uuid_workspace_key(prefix)
        && (suffix == "seq"
            || suffix
                .strip_prefix('p')
                .is_some_and(|idx| !idx.is_empty() && idx.chars().all(|ch| ch.is_ascii_digit())))
}

pub fn is_legacy_path_schema_name(schema: &str) -> bool {
    schema.len() == 17
        && schema.starts_with('h')
        && schema[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PgSchemaCleanupAction {
    Keep,
    DropCandidate,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgSchemaCleanupCandidate {
    pub schema_name: String,
    pub registered: bool,
    pub owner_kind: Option<String>,
    pub owner_key: Option<String>,
    pub owner_path: Option<String>,
    pub retention_class: Option<String>,
    pub table_count: i64,
    pub estimated_row_count: i64,
    pub action: PgSchemaCleanupAction,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgSchemaCleanupPlan {
    pub candidates: Vec<PgSchemaCleanupCandidate>,
}

impl PgSchemaCleanupPlan {
    pub fn drop_candidates(&self) -> impl Iterator<Item = &PgSchemaCleanupCandidate> {
        self.candidates
            .iter()
            .filter(|candidate| candidate.action == PgSchemaCleanupAction::DropCandidate)
    }

    pub fn blocked_unregistered(&self) -> impl Iterator<Item = &PgSchemaCleanupCandidate> {
        self.candidates.iter().filter(|candidate| {
            !candidate.registered && candidate.action == PgSchemaCleanupAction::Blocked
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgSchemaDropResult {
    pub schema_name: String,
    pub registered: bool,
    pub dropped: bool,
}

struct PgSchemaInventoryRow {
    schema_name: String,
    owner_kind: Option<String>,
    owner_key: Option<String>,
    owner_path: Option<String>,
    retention_class: Option<String>,
    table_count: i64,
    estimated_row_count: i64,
}

pub async fn ensure_pg_schema_registry(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{PG_SCHEMA_REGISTRY_SCHEMA}\""
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\" (
            schema_name     TEXT PRIMARY KEY,
            owner_kind      TEXT NOT NULL,
            owner_key       TEXT NOT NULL,
            owner_path      TEXT,
            retention_class TEXT NOT NULL,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            last_seen_at    TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn register_pg_schema_ownership(
    pool: &PgPool,
    ownership: &PgSchemaOwnership,
) -> anyhow::Result<()> {
    crate::db_pg::validate_schema_name(&ownership.schema_name)?;
    ensure_pg_schema_registry(pool).await?;
    sqlx::query(&format!(
        "INSERT INTO \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"
            (schema_name, owner_kind, owner_key, owner_path, retention_class)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (schema_name) DO UPDATE SET
            owner_kind = EXCLUDED.owner_kind,
            owner_key = EXCLUDED.owner_key,
            owner_path = EXCLUDED.owner_path,
            retention_class = EXCLUDED.retention_class,
            last_seen_at = CURRENT_TIMESTAMP"
    ))
    .bind(&ownership.schema_name)
    .bind(&ownership.owner_kind)
    .bind(&ownership.owner_key)
    .bind(&ownership.owner_path)
    .bind(&ownership.retention_class)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pg_schema_cleanup_plan(
    pool: &PgPool,
    include_unregistered: bool,
) -> anyhow::Result<PgSchemaCleanupPlan> {
    let registry_exists = pg_schema_registry_exists(pool).await?;
    let owner_select = if registry_exists {
        "o.owner_kind,
            o.owner_key,
            o.owner_path,
            o.retention_class"
    } else {
        "NULL::TEXT AS owner_kind,
            NULL::TEXT AS owner_key,
            NULL::TEXT AS owner_path,
            NULL::TEXT AS retention_class"
    };
    let registry_join = if registry_exists {
        format!(
            "LEFT JOIN \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\" o
               ON o.schema_name = n.nspname"
        )
    } else {
        String::new()
    };
    let owner_group_by = if registry_exists {
        ", o.owner_kind, o.owner_key, o.owner_path, o.retention_class"
    } else {
        ""
    };
    let rows = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
        ),
    >(&format!(
        "SELECT
            n.nspname AS schema_name,
            {owner_select},
            COUNT(c.oid) FILTER (WHERE c.relkind IN ('r', 'p'))::BIGINT AS table_count,
            COALESCE(SUM(GREATEST(c.reltuples, 0::REAL)), 0)::BIGINT AS estimated_row_count
         FROM pg_namespace n
         LEFT JOIN pg_class c
           ON c.relnamespace = n.oid AND c.relkind IN ('r', 'p')
         {registry_join}
         WHERE n.nspname ~ '^h[0-9a-f]{{16}}$'
         GROUP BY n.nspname{owner_group_by}
         ORDER BY n.nspname"
    ))
    .fetch_all(pool)
    .await?;

    let candidates = rows
        .into_iter()
        .filter_map(
            |(
                schema_name,
                owner_kind,
                owner_key,
                owner_path,
                retention_class,
                table_count,
                estimated_row_count,
            )| {
                if owner_kind.is_none() && !include_unregistered {
                    return None;
                }
                let row = PgSchemaInventoryRow {
                    schema_name,
                    owner_kind,
                    owner_key,
                    owner_path,
                    retention_class,
                    table_count,
                    estimated_row_count,
                };
                Some(classify_schema_cleanup_candidate(row))
            },
        )
        .collect();

    Ok(PgSchemaCleanupPlan { candidates })
}

async fn pg_schema_registry_exists(pool: &PgPool) -> anyhow::Result<bool> {
    let qualified_name = format!("\"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"");
    let table_name: Option<String> = sqlx::query_scalar("SELECT to_regclass($1::TEXT)::TEXT")
        .bind(qualified_name)
        .fetch_one(pool)
        .await?;
    Ok(table_name.is_some())
}

pub async fn apply_pg_schema_cleanup(
    pool: &PgPool,
    registered_schemas: &[String],
    allow_unregistered: &[String],
) -> anyhow::Result<Vec<PgSchemaDropResult>> {
    let requested_registered: HashSet<String> = registered_schemas.iter().cloned().collect();
    let allowlist: HashSet<String> = allow_unregistered.iter().cloned().collect();
    for schema in &requested_registered {
        crate::db_pg::validate_schema_name(schema)?;
    }
    for schema in &allowlist {
        crate::db_pg::validate_schema_name(schema)?;
    }

    let plan = pg_schema_cleanup_plan(pool, true).await?;
    let targets = validate_pg_schema_drop_request(plan, &requested_registered, &allowlist)?;

    let mut results = Vec::new();
    for candidate in targets {
        drop_schema_cascade(pool, &candidate.schema_name).await?;
        if candidate.registered {
            delete_schema_ownership(pool, &candidate.schema_name).await?;
        }
        results.push(PgSchemaDropResult {
            schema_name: candidate.schema_name,
            registered: candidate.registered,
            dropped: true,
        });
    }
    Ok(results)
}

fn validate_pg_schema_drop_request(
    plan: PgSchemaCleanupPlan,
    requested_registered: &HashSet<String>,
    allowlist: &HashSet<String>,
) -> anyhow::Result<Vec<PgSchemaCleanupCandidate>> {
    let mut targets = Vec::new();
    let mut seen_registered = HashSet::new();
    let mut seen_allowlisted = HashSet::new();
    for candidate in plan.candidates {
        let requested_registered_drop =
            candidate.registered && requested_registered.contains(&candidate.schema_name);
        let allowed_unregistered =
            !candidate.registered && allowlist.contains(&candidate.schema_name);
        if requested_registered_drop {
            seen_registered.insert(candidate.schema_name.clone());
            if candidate.action != PgSchemaCleanupAction::DropCandidate {
                anyhow::bail!(
                    "registered schema '{}' is not a cleanup drop candidate: {}",
                    candidate.schema_name,
                    candidate.reason
                );
            }
        }
        if allowed_unregistered {
            seen_allowlisted.insert(candidate.schema_name.clone());
        }
        if !requested_registered_drop && !allowed_unregistered {
            continue;
        }
        targets.push(candidate);
    }
    if let Some(schema) = requested_registered.difference(&seen_registered).next() {
        anyhow::bail!(
            "registered schema '{}' was not found in cleanup plan",
            schema
        );
    }
    if let Some(schema) = allowlist.difference(&seen_allowlisted).next() {
        anyhow::bail!(
            "allowlisted unregistered schema '{}' was not found in cleanup plan",
            schema
        );
    }
    Ok(targets)
}

fn classify_schema_cleanup_candidate(row: PgSchemaInventoryRow) -> PgSchemaCleanupCandidate {
    let registered = row.owner_kind.is_some();
    let (action, reason) = if !registered {
        (
            PgSchemaCleanupAction::Blocked,
            "unregistered path-derived schema; cleanup requires explicit allowlist".to_string(),
        )
    } else if row
        .owner_kind
        .as_deref()
        .is_some_and(is_path_derived_owner_kind)
    {
        (
            PgSchemaCleanupAction::Keep,
            "registered path-derived schema; owner_path is an identity key, not a liveness check"
                .to_string(),
        )
    } else if let Some(owner_path) = row.owner_path.as_deref() {
        match Path::new(owner_path).try_exists() {
            Ok(true) => (
                PgSchemaCleanupAction::Keep,
                "registered owner path still exists".to_string(),
            ),
            Ok(false) => (
                PgSchemaCleanupAction::DropCandidate,
                "registered owner path is missing".to_string(),
            ),
            Err(error) => (
                PgSchemaCleanupAction::Keep,
                format!("could not check registered owner path; keep for operator review: {error}"),
            ),
        }
    } else {
        (
            PgSchemaCleanupAction::Keep,
            "registered schema has no owner_path; keep for operator review".to_string(),
        )
    };

    PgSchemaCleanupCandidate {
        schema_name: row.schema_name,
        registered,
        owner_kind: row.owner_kind,
        owner_key: row.owner_key,
        owner_path: row.owner_path,
        retention_class: row.retention_class,
        table_count: row.table_count,
        estimated_row_count: row.estimated_row_count,
        action,
        reason,
    }
}

async fn drop_schema_cascade(pool: &PgPool, schema: &str) -> anyhow::Result<()> {
    sqlx::query(&drop_schema_cascade_statement(schema, false)?)
        .execute(pool)
        .await?;
    Ok(())
}

async fn drop_schema_cascade_if_exists(pool: &PgPool, schema: &str) -> anyhow::Result<()> {
    sqlx::query(&drop_schema_cascade_statement(schema, true)?)
        .execute(pool)
        .await?;
    Ok(())
}

fn drop_schema_cascade_statement(schema: &str, if_exists: bool) -> anyhow::Result<String> {
    crate::db_pg::validate_schema_name(schema)?;
    let if_exists = if if_exists { " IF EXISTS" } else { "" };
    Ok(format!("DROP SCHEMA{if_exists} \"{schema}\" CASCADE"))
}

async fn delete_schema_ownership(pool: &PgPool, schema: &str) -> anyhow::Result<()> {
    sqlx::query(&format!(
        "DELETE FROM \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"
         WHERE schema_name = $1"
    ))
    .bind(schema)
    .execute(pool)
    .await?;
    Ok(())
}

/// Report from a reap pass over orphaned path-derived schemas.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PgSchemaReapReport {
    /// Number of registered path-derived schemas examined.
    pub scanned: usize,
    /// Schemas whose owning workspace directory no longer exists (orphans).
    pub orphans: Vec<String>,
    /// `true` if the orphans were dropped, `false` for a dry run.
    pub dropped: bool,
}

/// True if a path-derived schema's owner store path is orphaned.
///
/// File-style logical paths use the directory that contained the store (the owner
/// path's parent) as the liveness signal. Directory owner paths are registered
/// separately and use the owner path itself as the liveness signal, so deleting a
/// workspace directory whose parent still exists is still detected as orphaned.
/// Older registry rows did not distinguish directory owners, so generated
/// workspace-key children of known workspace roots are treated as legacy
/// directory owners.
///
/// Only a definitive `NotFound` counts as orphaned. Any other error reading the
/// liveness path metadata (permissions, a transiently-unavailable mount, etc.) is
/// treated as "keep", so a transient IO failure can never cause a destructive
/// drop of a live schema. `metadata` (not `Path::exists`) is used precisely so
/// these two cases can be distinguished.
#[cfg(test)]
fn owner_path_is_orphaned(owner_kind: &str, owner_path: &Path) -> bool {
    owner_path_is_orphaned_with_workspace_roots(owner_kind, owner_path, &[])
}

fn owner_path_is_orphaned_with_workspace_roots(
    owner_kind: &str,
    owner_path: &Path,
    workspace_roots: &[PathBuf],
) -> bool {
    let liveness_path = if owner_kind == PATH_DERIVED_DIRECTORY_OWNER_KIND
        || (owner_kind == PATH_DERIVED_OWNER_KIND
            && is_legacy_workspace_directory_owner_path(owner_path, workspace_roots))
    {
        owner_path
    } else {
        let Some(parent) = owner_path.parent() else {
            return false;
        };
        parent
    };
    match std::fs::metadata(liveness_path) {
        Ok(_) => false,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

fn orphaned_path_schema_names(
    rows: Vec<(String, String, Option<String>)>,
    workspace_roots: Vec<PathBuf>,
) -> Vec<String> {
    let mut orphans = Vec::new();
    for (schema_name, owner_kind, owner_path) in rows {
        // No recorded path: cannot prove the schema is orphaned, so keep it.
        let Some(owner_path) = owner_path else {
            continue;
        };
        if owner_path_is_orphaned_with_workspace_roots(
            &owner_kind,
            Path::new(&owner_path),
            &workspace_roots,
        ) {
            orphans.push(schema_name);
        }
    }
    orphans
}

/// Drop registered path-derived schemas whose owning workspace directory is gone.
///
/// This bounds Postgres catalog growth automatically: per-job / ephemeral stores
/// create a schema under their workspace path, and once the workspace is removed
/// the schema becomes a dead orphan that nothing can reopen. Reaping drops only
/// those orphans (the owner directory or file owner parent is missing) and the
/// matching registry rows — never a live store's schema.
///
/// With `apply = false` it reports what would be reaped (dry run) without
/// dropping anything. Must run on the host that owns the workspace paths.
pub async fn reap_orphaned_path_schemas(
    pool: &PgPool,
    apply: bool,
) -> anyhow::Result<PgSchemaReapReport> {
    reap_orphaned_path_schemas_with_workspace_roots(pool, apply, &[]).await
}

pub async fn reap_orphaned_path_schemas_with_workspace_roots(
    pool: &PgPool,
    apply: bool,
    workspace_roots: &[PathBuf],
) -> anyhow::Result<PgSchemaReapReport> {
    ensure_pg_schema_registry(pool).await?;
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(&format!(
        "SELECT schema_name, owner_kind, owner_path
         FROM \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"
         WHERE owner_kind IN ($1, $2)"
    ))
    .bind(PATH_DERIVED_OWNER_KIND)
    .bind(PATH_DERIVED_DIRECTORY_OWNER_KIND)
    .fetch_all(pool)
    .await?;

    let scanned = rows.len();
    let workspace_roots = workspace_roots.to_vec();
    let orphans = tokio::task::spawn_blocking(move || {
        let workspace_roots: Vec<PathBuf> = workspace_roots
            .iter()
            .map(|root| normalize_path_for_comparison(root))
            .collect();
        orphaned_path_schema_names(rows, workspace_roots)
    })
    .await?;

    if apply {
        for schema_name in &orphans {
            // Reaping is registry-driven, so stale rows may point at schemas
            // already removed by manual cleanup or an interrupted prior run.
            // Delete those ownership rows and keep processing later orphans.
            drop_schema_cascade_if_exists(pool, schema_name).await?;
            delete_schema_ownership(pool, schema_name).await?;
        }
    }

    Ok(PgSchemaReapReport {
        scanned,
        orphans,
        dropped: apply,
    })
}

#[cfg(test)]
#[path = "db_pg_schema_registry_tests.rs"]
mod db_pg_schema_registry_tests;
