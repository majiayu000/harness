use serde::Serialize;
use sqlx::postgres::PgPool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const PG_SCHEMA_REGISTRY_SCHEMA: &str = "harness_admin";
pub const PG_SCHEMA_REGISTRY_TABLE: &str = "schema_ownership";

const PATH_DERIVED_OWNER_KIND: &str = "path_derived_store";
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
            owner_kind: PATH_DERIVED_OWNER_KIND.to_string(),
            owner_key: owner_path.clone(),
            owner_path: Some(owner_path),
            retention_class: PATH_DERIVED_RETENTION_CLASS.to_string(),
        })
    }
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
    } else if row.owner_kind.as_deref() == Some(PATH_DERIVED_OWNER_KIND) {
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
    crate::db_pg::validate_schema_name(schema)?;
    sqlx::query(&format!("DROP SCHEMA \"{}\" CASCADE", schema))
        .execute(pool)
        .await?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_derived_ownership_records_canonical_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tasks.db");
        let ownership =
            PgSchemaOwnership::path_derived("h1234567890abcdef".to_string(), path.clone())?;

        assert_eq!(ownership.schema_name, "h1234567890abcdef");
        assert_eq!(ownership.owner_kind, "path_derived_store");
        assert_eq!(ownership.owner_key, path.to_string_lossy());
        assert_eq!(
            ownership.owner_path.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        assert_eq!(ownership.retention_class, "path_derived");
        Ok(())
    }

    #[test]
    fn cleanup_keeps_path_derived_schema_when_owner_path_exists() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let row = PgSchemaInventoryRow {
            schema_name: "h1111111111111111".to_string(),
            owner_kind: Some("path_derived_store".to_string()),
            owner_key: Some(dir.path().to_string_lossy().to_string()),
            owner_path: Some(dir.path().to_string_lossy().to_string()),
            retention_class: Some("path_derived".to_string()),
            table_count: 2,
            estimated_row_count: 5,
        };

        let candidate = classify_schema_cleanup_candidate(row);

        assert!(candidate.registered);
        assert_eq!(candidate.action, PgSchemaCleanupAction::Keep);
        assert_eq!(
            candidate.reason,
            "registered path-derived schema; owner_path is an identity key, not a liveness check"
        );
        Ok(())
    }

    #[test]
    fn cleanup_keeps_path_derived_schema_when_owner_path_is_missing() {
        let row = PgSchemaInventoryRow {
            schema_name: "h2222222222222222".to_string(),
            owner_kind: Some("path_derived_store".to_string()),
            owner_key: Some("/definitely/missing/harness/schema-owner".to_string()),
            owner_path: Some("/definitely/missing/harness/schema-owner".to_string()),
            retention_class: Some("path_derived".to_string()),
            table_count: 1,
            estimated_row_count: 0,
        };

        let candidate = classify_schema_cleanup_candidate(row);

        assert!(candidate.registered);
        assert_eq!(candidate.action, PgSchemaCleanupAction::Keep);
        assert_eq!(
            candidate.reason,
            "registered path-derived schema; owner_path is an identity key, not a liveness check"
        );
    }

    #[test]
    fn cleanup_marks_unknown_registered_schema_with_missing_path_as_drop_candidate() {
        let row = PgSchemaInventoryRow {
            schema_name: "h4444444444444444".to_string(),
            owner_kind: Some("external_owner".to_string()),
            owner_key: Some("/definitely/missing/harness/schema-owner".to_string()),
            owner_path: Some("/definitely/missing/harness/schema-owner".to_string()),
            retention_class: Some("path_derived".to_string()),
            table_count: 1,
            estimated_row_count: 0,
        };

        let candidate = classify_schema_cleanup_candidate(row);

        assert!(candidate.registered);
        assert_eq!(candidate.action, PgSchemaCleanupAction::DropCandidate);
        assert_eq!(candidate.reason, "registered owner path is missing");
    }

    #[test]
    fn cleanup_blocks_unregistered_schema_by_default() {
        let row = PgSchemaInventoryRow {
            schema_name: "h3333333333333333".to_string(),
            owner_kind: None,
            owner_key: None,
            owner_path: None,
            retention_class: None,
            table_count: 1,
            estimated_row_count: 0,
        };

        let candidate = classify_schema_cleanup_candidate(row);

        assert!(!candidate.registered);
        assert_eq!(candidate.action, PgSchemaCleanupAction::Blocked);
        assert!(candidate.reason.contains("explicit allowlist"));
    }

    #[test]
    fn drop_request_validation_rejects_registered_keep_candidate() {
        let plan = PgSchemaCleanupPlan {
            candidates: vec![PgSchemaCleanupCandidate {
                schema_name: "h1111111111111111".to_string(),
                registered: true,
                owner_kind: Some("path_derived_store".to_string()),
                owner_key: Some("h1111111111111111".to_string()),
                owner_path: None,
                retention_class: Some("path_derived".to_string()),
                table_count: 1,
                estimated_row_count: 0,
                action: PgSchemaCleanupAction::Keep,
                reason: "registered path-derived schema".to_string(),
            }],
        };
        let requested_registered = HashSet::from(["h1111111111111111".to_string()]);
        let allowlist = HashSet::new();

        let err = validate_pg_schema_drop_request(plan, &requested_registered, &allowlist)
            .expect_err("keep schema should be rejected");

        assert!(err
            .to_string()
            .contains("registered schema 'h1111111111111111' is not a cleanup drop candidate"));
    }

    #[test]
    fn drop_request_validation_fails_before_selecting_partial_targets() {
        let plan = PgSchemaCleanupPlan {
            candidates: vec![PgSchemaCleanupCandidate {
                schema_name: "h1111111111111111".to_string(),
                registered: true,
                owner_kind: Some("external_owner".to_string()),
                owner_key: Some("/definitely/missing/harness/schema-owner".to_string()),
                owner_path: Some("/definitely/missing/harness/schema-owner".to_string()),
                retention_class: Some("path_derived".to_string()),
                table_count: 1,
                estimated_row_count: 0,
                action: PgSchemaCleanupAction::DropCandidate,
                reason: "registered owner path is missing".to_string(),
            }],
        };
        let requested_registered = HashSet::from([
            "h1111111111111111".to_string(),
            "h9999999999999999".to_string(),
        ]);
        let allowlist = HashSet::new();

        let err = validate_pg_schema_drop_request(plan, &requested_registered, &allowlist)
            .expect_err("missing schema should reject the whole cleanup request");

        assert!(err
            .to_string()
            .contains("registered schema 'h9999999999999999' was not found"));
    }

    #[test]
    fn legacy_path_schema_name_detection_is_exact() {
        assert!(is_legacy_path_schema_name("h0123456789abcdef"));
        assert!(!is_legacy_path_schema_name("workflow_runtime"));
        assert!(!is_legacy_path_schema_name("h0123456789abcdeg"));
        assert!(!is_legacy_path_schema_name("h0123456789abcde"));
    }
}
