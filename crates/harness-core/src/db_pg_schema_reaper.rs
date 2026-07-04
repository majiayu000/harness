use serde::Serialize;
use sqlx::postgres::PgPool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{
    delete_schema_ownership, drop_schema_cascade_if_exists, ensure_pg_schema_registry,
    is_legacy_workspace_directory_owner_path, normalize_path_for_comparison,
    pg_schema_cleanup_plan, LEGACY_PATH_DERIVED_SCHEMA_OWNER_KIND,
    PATH_DERIVED_DIRECTORY_OWNER_KIND, PATH_DERIVED_OWNER_KIND, PG_SCHEMA_REGISTRY_SCHEMA,
    PG_SCHEMA_REGISTRY_TABLE,
};

pub const DEFAULT_ORPHAN_REAPER_LEGACY_BATCH: usize = 200;

const LEGACY_WORKSPACE_STORE_PATHS: &[&str] = &[
    "tasks.db",
    "threads.db",
    "projects.db",
    "plans.db",
    "issue_workflows.db",
    "project_workflows.db",
    "evals.db",
    "reviews.db",
    "q_values.db",
    "runtime_state.db",
    "events.db",
    "workflow_runtime.db",
    "workspace-leases",
];

/// Report from a reap pass over orphaned path-derived schemas.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PgSchemaReapReport {
    /// Total number of path-derived schemas examined.
    pub scanned: usize,
    /// Schemas whose owning workspace directory no longer exists.
    pub orphans: Vec<String>,
    /// `true` if the orphans were dropped, `false` for a dry run.
    pub dropped: bool,
    /// Number of registered path-derived schemas examined.
    pub registered_scanned: usize,
    /// Number of registered path-derived schemas selected for reaping.
    pub registered_reaped: usize,
    /// Number of unregistered legacy path-derived schemas examined.
    pub legacy_scanned: usize,
    /// Number of unregistered legacy path-derived schemas selected for reaping.
    pub legacy_reaped: usize,
    /// Non-fatal legacy scan errors. Any entry means legacy reaping was skipped
    /// for this tick, but registered reaping may still have proceeded.
    pub legacy_scan_errors: Vec<String>,
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
pub(crate) fn owner_path_is_orphaned(owner_kind: &str, owner_path: &Path) -> bool {
    owner_path_is_orphaned_with_workspace_roots(owner_kind, owner_path, &[])
}

pub(crate) fn owner_path_is_orphaned_with_workspace_roots(
    owner_kind: &str,
    owner_path: &Path,
    workspace_roots: &[PathBuf],
) -> bool {
    let liveness_path = if owner_kind == PATH_DERIVED_DIRECTORY_OWNER_KIND
        || ((owner_kind == PATH_DERIVED_OWNER_KIND
            || owner_kind == LEGACY_PATH_DERIVED_SCHEMA_OWNER_KIND)
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

pub(crate) fn orphaned_path_schema_names(
    rows: Vec<(String, String, Option<String>, Option<String>)>,
    workspace_roots: Vec<PathBuf>,
) -> Vec<String> {
    let mut orphans = Vec::new();
    for (schema_name, owner_kind, owner_key, owner_path) in rows {
        // No recorded path: cannot prove the schema is orphaned, so keep it.
        let Some(liveness_path) =
            owner_liveness_path(&owner_kind, owner_key.as_deref(), owner_path.as_deref())
        else {
            continue;
        };
        if owner_path_is_orphaned_with_workspace_roots(
            &owner_kind,
            Path::new(liveness_path),
            &workspace_roots,
        ) {
            orphans.push(schema_name);
        }
    }
    orphans
}

fn owner_liveness_path<'a>(
    owner_kind: &str,
    owner_key: Option<&'a str>,
    owner_path: Option<&'a str>,
) -> Option<&'a str> {
    owner_path.or_else(|| {
        if owner_kind == LEGACY_PATH_DERIVED_SCHEMA_OWNER_KIND {
            owner_key.filter(|key| Path::new(key).is_absolute())
        } else {
            None
        }
    })
}

/// Drop path-derived schemas whose owning workspace directory is gone.
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
    reap_orphaned_path_schemas_with_legacy_options(
        pool,
        apply,
        workspace_roots,
        true,
        DEFAULT_ORPHAN_REAPER_LEGACY_BATCH,
    )
    .await
}

pub async fn reap_orphaned_path_schemas_with_legacy_options(
    pool: &PgPool,
    apply: bool,
    workspace_roots: &[PathBuf],
    legacy_enabled: bool,
    legacy_batch: usize,
) -> anyhow::Result<PgSchemaReapReport> {
    ensure_pg_schema_registry(pool).await?;
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(&format!(
        "SELECT schema_name, owner_kind, owner_key, owner_path
         FROM \"{PG_SCHEMA_REGISTRY_SCHEMA}\".\"{PG_SCHEMA_REGISTRY_TABLE}\"
         WHERE owner_kind IN ($1, $2, $3)"
    ))
    .bind(PATH_DERIVED_OWNER_KIND)
    .bind(PATH_DERIVED_DIRECTORY_OWNER_KIND)
    .bind(LEGACY_PATH_DERIVED_SCHEMA_OWNER_KIND)
    .fetch_all(pool)
    .await?;

    let registered_scanned = rows.len();
    let registered_workspace_roots = workspace_roots.to_vec();
    let registered_orphans = tokio::task::spawn_blocking(move || {
        let workspace_roots: Vec<PathBuf> = registered_workspace_roots
            .iter()
            .map(|root| normalize_path_for_comparison(root))
            .collect();
        orphaned_path_schema_names(rows, workspace_roots)
    })
    .await?;
    let registered_reaped = registered_orphans.len();

    let legacy_scan = if legacy_enabled {
        let legacy_workspace_roots = workspace_roots.to_vec();
        let legacy_candidates = unregistered_legacy_path_schema_names(pool).await?;
        tokio::task::spawn_blocking(move || {
            legacy_orphaned_path_schema_names(
                legacy_candidates,
                &legacy_workspace_roots,
                legacy_batch,
            )
        })
        .await?
    } else {
        LegacyPathSchemaScan::disabled()
    };
    let legacy_scanned = legacy_scan.scanned;
    let legacy_reaped = legacy_scan.orphans.len();
    let legacy_scan_errors = legacy_scan.errors;
    let legacy_orphans = legacy_scan.orphans;

    if apply {
        for schema_name in &registered_orphans {
            // Reaping is registry-driven, so stale rows may point at schemas
            // already removed by manual cleanup or an interrupted prior run.
            // Delete those ownership rows and keep processing later orphans.
            drop_schema_cascade_if_exists(pool, schema_name).await?;
            delete_schema_ownership(pool, schema_name).await?;
        }
        for schema_name in &legacy_orphans {
            drop_schema_cascade_if_exists(pool, schema_name).await?;
        }
    }

    let scanned = registered_scanned + legacy_scanned;
    let mut orphans = registered_orphans;
    orphans.extend(legacy_orphans);

    Ok(PgSchemaReapReport {
        scanned,
        orphans,
        dropped: apply,
        registered_scanned,
        registered_reaped,
        legacy_scanned,
        legacy_reaped,
        legacy_scan_errors,
    })
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LegacyPathSchemaScan {
    pub scanned: usize,
    pub orphans: Vec<String>,
    pub errors: Vec<String>,
}

impl LegacyPathSchemaScan {
    fn disabled() -> Self {
        Self::default()
    }
}

async fn unregistered_legacy_path_schema_names(pool: &PgPool) -> anyhow::Result<Vec<String>> {
    let plan = pg_schema_cleanup_plan(pool, true).await?;
    Ok(plan
        .candidates
        .into_iter()
        .filter(|candidate| !candidate.registered)
        .map(|candidate| candidate.schema_name)
        .collect())
}

pub(crate) fn legacy_orphaned_path_schema_names(
    candidates: Vec<String>,
    workspace_roots: &[PathBuf],
    legacy_batch: usize,
) -> LegacyPathSchemaScan {
    let scanned = candidates.len();
    if candidates.is_empty() || legacy_batch == 0 {
        return LegacyPathSchemaScan {
            scanned,
            ..LegacyPathSchemaScan::default()
        };
    }

    let live_schemas = match live_legacy_workspace_schema_names(workspace_roots) {
        Ok(live_schemas) => live_schemas,
        Err(error) => {
            return LegacyPathSchemaScan {
                scanned,
                errors: vec![error.to_string()],
                ..LegacyPathSchemaScan::default()
            };
        }
    };

    let orphans = candidates
        .into_iter()
        .filter(|schema| !live_schemas.contains(schema))
        .take(legacy_batch)
        .collect();

    LegacyPathSchemaScan {
        scanned,
        orphans,
        errors: Vec::new(),
    }
}

pub(crate) fn live_legacy_workspace_schema_names(
    workspace_roots: &[PathBuf],
) -> anyhow::Result<HashSet<String>> {
    if workspace_roots.is_empty() {
        anyhow::bail!("no workspace roots configured for legacy path-schema reaping");
    }

    let mut schemas = HashSet::new();
    for root in workspace_roots {
        let entries = std::fs::read_dir(root).map_err(|error| {
            anyhow::anyhow!("failed to read workspace root {:?}: {error}", root)
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                anyhow::anyhow!(
                    "failed to read workspace root entry under {:?}: {error}",
                    root
                )
            })?;
            let file_type = entry.file_type().map_err(|error| {
                anyhow::anyhow!(
                    "failed to inspect workspace root entry {:?}: {error}",
                    entry.path()
                )
            })?;
            if !file_type.is_dir() {
                continue;
            }

            let workspace_path = entry.path();
            schemas.insert(crate::db_pg::pg_schema_for_path(&workspace_path)?);
            for store_path in LEGACY_WORKSPACE_STORE_PATHS {
                schemas.insert(crate::db_pg::pg_schema_for_path(
                    &workspace_path.join(store_path),
                )?);
            }
        }
    }

    Ok(schemas)
}
