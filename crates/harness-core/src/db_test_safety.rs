use serde::Serialize;
use sqlx::postgres::{PgConnectOptions, PgPool};
use std::collections::HashSet;
use std::str::FromStr as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::db_pg::{pg_open_pool, validate_schema_name};
use crate::db_pg_schema_registry::PgSchemaDropResult;

pub const ALLOW_NON_TEST_DATABASE_FOR_TESTS_ENV: &str = "HARNESS_ALLOW_NON_TEST_DATABASE_FOR_TESTS";

pub const KNOWN_TEST_SCHEMA_PREFIXES: &[&str] = &[
    "event_store_backfill_test",
    "event_store_jsonl_test",
    "event_store_scope_test",
    "plan_db_backfill_test",
    "project_registry_migration_test",
    "project_registry_scope_test",
    "review_store_test",
    "runtime_state_store_test",
    "task_db_backfill_test",
    "task_db_retention_batch_test",
    "task_db_retention_scope_test",
    "task_db_scope_test",
    "thread_db_shared_scope_test",
    "thread_db_test",
    "workspace_lease_scope_test",
];

static TEST_SCHEMA_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn is_test_database_url(database_url: &str) -> bool {
    test_database_name(database_url).is_some_and(|name| is_test_database_name(&name))
}

pub fn validate_test_database_url(database_url: &str) -> anyhow::Result<()> {
    if non_test_database_override_enabled() || is_test_database_url(database_url) {
        return Ok(());
    }

    let database_name = test_database_name(database_url).unwrap_or_else(|| "<unknown>".to_string());
    anyhow::bail!(
        "refusing to run Postgres-backed tests against non-test database '{}' from {}; use a database named harness_test, ending in _test, starting with test_, or set {}=1 only for a disposable database",
        database_name,
        redact_database_url(database_url),
        ALLOW_NON_TEST_DATABASE_FOR_TESTS_ENV
    )
}

pub fn resolve_test_database_url(configured_database_url: Option<&str>) -> anyhow::Result<String> {
    let database_url = crate::db_pg::resolve_database_url(configured_database_url)?;
    validate_test_database_url(&database_url)?;
    Ok(database_url)
}

pub fn redact_database_url(database_url: &str) -> String {
    let trimmed = database_url.trim();
    let without_query = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .to_string();
    let Some(scheme_end) = without_query.find("://") else {
        return "<redacted database URL>".to_string();
    };
    let authority_start = scheme_end + 3;
    let Some(path_start) = without_query[authority_start..].find('/') else {
        return without_query;
    };
    let path_start = authority_start + path_start;
    let authority = &without_query[authority_start..path_start];
    let redacted_authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("<redacted>@{host}"),
        None => authority.to_string(),
    };
    format!(
        "{}{}{}",
        &without_query[..authority_start],
        redacted_authority,
        &without_query[path_start..]
    )
}

pub fn is_known_test_schema_name(schema: &str) -> bool {
    KNOWN_TEST_SCHEMA_PREFIXES.iter().any(|prefix| {
        generated_test_schema_matches_prefix(schema, prefix)
            || legacy_single_suffix_test_schema_matches_prefix(schema, prefix)
    })
}

pub fn unique_test_schema_name(prefix: &str) -> anyhow::Result<String> {
    if !KNOWN_TEST_SCHEMA_PREFIXES.contains(&prefix) {
        anyhow::bail!("unknown test schema prefix: {prefix}");
    }
    let count = TEST_SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| anyhow::anyhow!("system clock before UNIX epoch: {error}"))?
        .as_nanos();
    let schema = format!("{prefix}_{nanos}_{count}");
    validate_schema_name(&schema)?;
    Ok(schema)
}

pub async fn drop_test_schema_if_exists(pool: &PgPool, schema: &str) -> anyhow::Result<()> {
    validate_schema_name(schema)?;
    if !is_known_test_schema_name(schema) {
        anyhow::bail!("refusing to drop unknown test schema: {schema}");
    }
    sqlx::query(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .execute(pool)
        .await?;
    Ok(())
}

pub struct TestSchemaGuard {
    database_url: String,
    schema_name: String,
    cleaned: bool,
}

impl TestSchemaGuard {
    pub fn new(database_url: impl Into<String>, prefix: &str) -> anyhow::Result<Self> {
        let database_url = database_url.into();
        validate_test_database_url(&database_url)?;
        Ok(Self {
            database_url,
            schema_name: unique_test_schema_name(prefix)?,
            cleaned: false,
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema_name
    }

    pub async fn cleanup_with_pool(&mut self, pool: &PgPool) -> anyhow::Result<()> {
        drop_test_schema_if_exists(pool, &self.schema_name).await?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for TestSchemaGuard {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let database_url = self.database_url.clone();
        let schema = self.schema_name.clone();
        let result = std::thread::Builder::new()
            .name("harness-test-schema-cleanup".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    let pool = pg_open_pool(&database_url).await?;
                    let result = drop_test_schema_if_exists(&pool, &schema).await;
                    pool.close().await;
                    result
                })
            })
            .and_then(|handle| {
                handle
                    .join()
                    .map_err(|_| std::io::Error::other("test schema cleanup thread panicked"))?
                    .map_err(std::io::Error::other)
            });
        if let Err(error) = result {
            tracing::warn!(schema = %self.schema_name, error = %error, "failed to clean up test schema");
        } else {
            self.cleaned = true;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgTestSchemaCleanupCandidate {
    pub schema_name: String,
    pub table_count: i64,
    pub estimated_row_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgTestSchemaCleanupPlan {
    pub candidates: Vec<PgTestSchemaCleanupCandidate>,
}

pub async fn pg_test_schema_cleanup_plan(pool: &PgPool) -> anyhow::Result<PgTestSchemaCleanupPlan> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT
            n.nspname AS schema_name,
            COUNT(c.oid) FILTER (WHERE c.relkind IN ('r', 'p'))::BIGINT AS table_count,
            COALESCE(SUM(GREATEST(c.reltuples, 0::REAL)), 0)::BIGINT AS estimated_row_count
         FROM pg_namespace n
         LEFT JOIN pg_class c
           ON c.relnamespace = n.oid AND c.relkind IN ('r', 'p')
         WHERE n.nspname LIKE '%\\_test\\_%' ESCAPE '\\'
         GROUP BY n.nspname
         ORDER BY n.nspname",
    )
    .fetch_all(pool)
    .await?;

    let candidates = rows
        .into_iter()
        .filter(|(schema_name, _, _)| is_known_test_schema_name(schema_name))
        .map(
            |(schema_name, table_count, estimated_row_count)| PgTestSchemaCleanupCandidate {
                schema_name,
                table_count,
                estimated_row_count,
            },
        )
        .collect();
    Ok(PgTestSchemaCleanupPlan { candidates })
}

pub async fn apply_pg_test_schema_cleanup(
    pool: &PgPool,
    schemas: &[String],
) -> anyhow::Result<Vec<PgSchemaDropResult>> {
    let requested: HashSet<String> = schemas.iter().cloned().collect();
    for schema in &requested {
        validate_schema_name(schema)?;
        if !is_known_test_schema_name(schema) {
            anyhow::bail!("refusing to drop unknown test schema: {schema}");
        }
    }
    let plan = pg_test_schema_cleanup_plan(pool).await?;
    let available: HashSet<String> = plan
        .candidates
        .iter()
        .map(|candidate| candidate.schema_name.clone())
        .collect();
    if let Some(missing) = requested.difference(&available).next() {
        anyhow::bail!("test schema '{}' was not found in cleanup plan", missing);
    }

    let mut results = Vec::new();
    for schema in requested {
        drop_test_schema_if_exists(pool, &schema).await?;
        results.push(PgSchemaDropResult {
            schema_name: schema,
            registered: false,
            dropped: true,
        });
    }
    Ok(results)
}

fn test_database_name(database_url: &str) -> Option<String> {
    PgConnectOptions::from_str(database_url)
        .ok()
        .and_then(|options| options.get_database().map(ToOwned::to_owned))
}

fn is_test_database_name(name: &str) -> bool {
    name == "harness_test" || name.ends_with("_test") || name.starts_with("test_")
}

fn non_test_database_override_enabled() -> bool {
    std::env::var(ALLOW_NON_TEST_DATABASE_FOR_TESTS_ENV)
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn generated_test_schema_matches_prefix(schema: &str, prefix: &str) -> bool {
    let Some(rest) = schema
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return false;
    };
    let Some((nanos, counter)) = rest.rsplit_once('_') else {
        return false;
    };
    !nanos.is_empty()
        && !counter.is_empty()
        && nanos.chars().all(|ch| ch.is_ascii_digit())
        && counter.chars().all(|ch| ch.is_ascii_digit())
}

fn legacy_single_suffix_test_schema_matches_prefix(schema: &str, prefix: &str) -> bool {
    if prefix != "review_store_test" {
        return false;
    }
    let Some(rest) = schema
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::process_env_lock;

    #[test]
    fn test_database_url_detection_accepts_test_names() {
        assert!(is_test_database_url(
            "postgres://user:pass@localhost:5432/harness_test"
        ));
        assert!(is_test_database_url(
            "postgres://user:pass@localhost:5432/project_test"
        ));
        assert!(is_test_database_url(
            "postgres://user:pass@localhost:5432/test_project"
        ));
    }

    #[test]
    fn test_database_url_detection_rejects_primary_name() {
        assert!(!is_test_database_url(
            "postgres://user:pass@localhost:5432/harness"
        ));
    }

    #[test]
    fn unsafe_database_url_error_redacts_credentials_and_query() {
        let err = validate_test_database_url(
            "postgres://user:secret@localhost:5432/harness?sslmode=require",
        )
        .expect_err("primary database URL must be rejected");
        let message = err.to_string();
        assert!(message.contains("postgres://<redacted>@localhost:5432/harness"));
        assert!(!message.contains("user"));
        assert!(!message.contains("secret"));
        assert!(!message.contains("sslmode"));
    }

    #[test]
    fn explicit_escape_hatch_allows_non_test_database() {
        let _lock = process_env_lock();
        temp_env::with_var(ALLOW_NON_TEST_DATABASE_FOR_TESTS_ENV, Some("1"), || {
            validate_test_database_url("postgres://user:secret@localhost:5432/harness")
                .expect("explicit disposable-db override should allow non-test names");
        });
    }

    #[test]
    fn known_test_schema_matches_generated_shape_only() {
        assert!(is_known_test_schema_name(
            "event_store_scope_test_1783054126968028000_0"
        ));
        assert!(!is_known_test_schema_name("event_store_scope_test"));
        assert!(!is_known_test_schema_name(
            "event_store_scope_test_1783054126968028000"
        ));
        assert!(!is_known_test_schema_name(
            "unrelated_test_1783054126968028000_0"
        ));
    }

    #[test]
    fn known_test_schema_matches_legacy_single_numeric_suffix() {
        assert!(is_known_test_schema_name(
            "review_store_test_1783054126968028000"
        ));
        assert!(!is_known_test_schema_name("review_store_test_alpha"));
    }
}
