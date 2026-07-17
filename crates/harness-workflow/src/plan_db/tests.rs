use super::*;
use futures::FutureExt;
use harness_core::db::{pg_open_pool, pg_open_pool_schematized, resolve_database_url, PgMigrator};
use harness_core::types::ExecPlanStatus;
use std::sync::OnceLock;
use tokio::sync::{Semaphore, SemaphorePermit};

// Serialize plan_db tests to stay within the Supabase session-mode
// connection limit (15). Each pool uses up to 8 connections; concurrent
// tests would exhaust the limit. One test at a time uses ≤8 connections.
static DB_GATE: OnceLock<Semaphore> = OnceLock::new();
fn db_gate() -> &'static Semaphore {
    DB_GATE.get_or_init(|| Semaphore::new(1))
}

async fn open_test_store(
) -> anyhow::Result<Option<(PlanDb, tempfile::TempDir, SemaphorePermit<'static>)>> {
    if resolve_database_url(None).is_err() {
        return Ok(None);
    }
    let permit = db_gate().acquire().await?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("plans.db");
    let db = PlanDb::open(&path).await?;
    Ok(Some((db, dir, permit)))
}

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

#[test]
fn shared_schema_context_uses_fixed_plan_db_schema() -> anyhow::Result<()> {
    let context =
        PlanDb::shared_schema_context(Some("postgres://user:pass@localhost:5432/harness"))?;
    assert_eq!(context.schema(), PLAN_DB_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared plan_db schema must not register path-derived ownership"
    );
    Ok(())
}

#[test]
fn store_key_creates_data_dir_before_canonicalizing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("missing").join("data");
    assert!(!data_dir.exists());

    let store_key = PlanDb::store_key_for_data_dir(&data_dir)?;

    assert!(data_dir.is_dir());
    assert_eq!(
        store_key,
        data_dir.canonicalize()?.to_string_lossy().into_owned()
    );
    assert_eq!(store_key, PlanDb::store_key_for_data_dir(&data_dir)?);
    Ok(())
}

#[tokio::test]
async fn plan_db_roundtrip() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let plan = ExecPlan::from_spec("Test plan purpose", dir.path())?;
    db.upsert(&plan).await?;

    let loaded = db.get(&plan.id).await?.expect("plan should exist");
    assert_eq!(loaded.id.as_str(), plan.id.as_str());
    assert_eq!(loaded.purpose, plan.purpose);

    let all = db.list().await?;
    assert_eq!(all.len(), 1);
    Ok(())
}

#[tokio::test]
async fn plan_db_get_missing_returns_none() -> anyhow::Result<()> {
    let Some((db, _dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    assert!(db.get(&ExecPlanId::new()).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn plan_db_delete_removes_plan() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let plan = ExecPlan::from_spec("# Delete me", dir.path())?;
    db.upsert(&plan).await?;
    assert!(db.delete(&plan.id).await?);
    assert!(db.get(&plan.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn plan_db_delete_missing_returns_false() -> anyhow::Result<()> {
    let Some((db, _dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    assert!(!db.delete(&ExecPlanId::new()).await?);
    Ok(())
}

#[tokio::test]
async fn list_by_status_filters_correctly() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let draft = ExecPlan::from_spec("# Draft plan", dir.path())?;
    let mut active = ExecPlan::from_spec("# Active plan", dir.path())?;
    active.activate();
    let mut completed = ExecPlan::from_spec("# Completed plan", dir.path())?;
    completed.complete();

    db.upsert(&draft).await?;
    db.upsert(&active).await?;
    db.upsert(&completed).await?;

    let drafts = db.list_by_status(ExecPlanStatus::Draft).await?;
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].id.as_str(), draft.id.as_str());

    let actives = db.list_by_status(ExecPlanStatus::Active).await?;
    assert_eq!(actives.len(), 1);
    assert_eq!(actives[0].id.as_str(), active.id.as_str());
    Ok(())
}

#[tokio::test]
async fn search_by_name_finds_matching_plans() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let auth = ExecPlan::from_spec("# Implement authentication", dir.path())?;
    let deploy = ExecPlan::from_spec("# Deploy to production", dir.path())?;

    db.upsert(&auth).await?;
    db.upsert(&deploy).await?;

    let results = db.search_by_name("auth").await?;
    assert_eq!(results.len(), 1);
    assert!(results[0].purpose.to_lowercase().contains("auth"));

    let all = db.search_by_name("").await?;
    assert_eq!(all.len(), 2);
    Ok(())
}

#[tokio::test]
async fn migrate_from_markdown_dir_imports_plans() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let plan = ExecPlan::from_spec("# Migration test plan", dir.path())?;
    let md = plan.to_markdown();
    std::fs::write(dir.path().join("plan1.md"), &md)?;

    let count = db.migrate_from_markdown_dir(dir.path()).await?;
    assert_eq!(count, 1);

    let loaded = db.get(&plan.id).await?.expect("migrated plan should exist");
    assert_eq!(loaded.purpose, plan.purpose);

    let count2 = db.migrate_from_markdown_dir(dir.path()).await?;
    assert_eq!(count2, 0);
    Ok(())
}

#[tokio::test]
async fn migrate_from_markdown_dir_skips_nonexistent_dir() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let count = db
        .migrate_from_markdown_dir(&dir.path().join("nonexistent"))
        .await?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn update_in_txn_persists_mutation() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let plan = ExecPlan::from_spec("# Update test", dir.path())?;
    db.upsert(&plan).await?;

    let updated = db
        .update_in_txn(&plan.id, |p| p.activate())
        .await?
        .ok_or_else(|| anyhow::anyhow!("plan should exist after upsert"))?;
    assert_eq!(updated.status, ExecPlanStatus::Active);

    let reloaded = db
        .get(&plan.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("plan should still exist after update"))?;
    assert_eq!(reloaded.status, ExecPlanStatus::Active);
    Ok(())
}

#[tokio::test]
async fn update_in_txn_returns_none_for_missing_plan() -> anyhow::Result<()> {
    let Some((db, _dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    let result: Option<ExecPlan> = db
        .update_in_txn(&ExecPlanId::new(), |p| p.activate())
        .await?;
    assert!(result.is_none());
    Ok(())
}

#[tokio::test]
async fn upsert_sanitizes_nul_bytes() -> anyhow::Result<()> {
    let Some((db, dir, _permit)) = open_test_store().await? else {
        return Ok(());
    };
    // PostgreSQL JSONB rejects \u0000; a spec with an embedded NUL must not fail.
    let plan = ExecPlan::from_spec("# a\0b", dir.path())?;
    db.upsert(&plan).await?;
    let loaded = db
        .get(&plan.id)
        .await?
        .expect("plan should exist after nul-stripped upsert");
    assert!(
        loaded.purpose.contains('a'),
        "purpose should contain 'a' after NUL stripping"
    );
    Ok(())
}

/// Verifies the upgrade contract: a database with v1+v2 migrations (data TEXT)
/// is correctly upgraded to v3 (JSONB column) and v4 (GIN index), and that
/// pre-existing TEXT rows survive the `USING data::jsonb` cast.
#[tokio::test]
async fn legacy_plan_db_migration_backfills_once() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let _permit = db_gate().acquire().await?;

    let dir = tempfile::tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    let other_data_dir = dir.path().join("other-data");
    let legacy_path = target_data_dir.join("plans.db");
    let legacy_schema = PgStoreContext::from_legacy_path_schema(&legacy_path, Some(&database_url))?
        .schema()
        .to_owned();
    let target_schema = unique_test_schema("plan_db_backfill_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
    let target_db =
        PlanDb::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir).await?;
    let other_db =
        PlanDb::open_shared_with_data_dir(&target_context, &setup_pool, &other_data_dir).await?;
    let legacy_db = PlanDb::open_with_database_url(&legacy_path, Some(&database_url)).await?;

    let result = std::panic::AssertUnwindSafe(async {
        let mut legacy_plan = ExecPlan::from_spec("# Legacy plan", &target_data_dir)?;
        legacy_plan.purpose = "legacy plan".to_string();
        legacy_db.upsert(&legacy_plan).await?;

        let copied =
            migrate_legacy_plan_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
        assert_eq!(copied, 1, "one legacy plan should be copied");

        let copied_again =
            migrate_legacy_plan_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
        assert_eq!(copied_again, 0, "migration must be idempotent");

        let loaded = target_db
            .get(&legacy_plan.id)
            .await?
            .expect("legacy plan should be present in the shared schema");
        assert_eq!(loaded.purpose, "legacy plan");
        assert!(
            other_db.get(&legacy_plan.id).await?.is_none(),
            "other data_dir scopes must not hydrate legacy plans"
        );

        let mut shared_plan = loaded;
        shared_plan.purpose = "shared update".to_string();
        target_db.upsert(&shared_plan).await?;
        let copied_after_shared_update =
            migrate_legacy_plan_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
        assert_eq!(
            copied_after_shared_update, 0,
            "completed migration must not overwrite updated shared plans"
        );
        let updated = target_db
            .get(&legacy_plan.id)
            .await?
            .expect("updated shared plan should remain");
        assert_eq!(updated.purpose, "shared update");

        target_db.delete(&legacy_plan.id).await?;
        let copied_after_shared_delete =
            migrate_legacy_plan_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
        assert_eq!(
            copied_after_shared_delete, 0,
            "completed migration must not resurrect deleted shared plans"
        );
        assert!(
            target_db.get(&legacy_plan.id).await?.is_none(),
            "deleted shared plan must stay deleted"
        );
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    legacy_db.pool.close().await;
    target_db.pool.close().await;
    other_db.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn migration_contract_text_to_jsonb_gin() -> anyhow::Result<()> {
    let Ok(database_url) = resolve_database_url(None) else {
        return Ok(());
    };
    let _permit = db_gate().acquire().await?;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("migrate_contract");
    use sha2::{Digest, Sha256};
    let path_utf8 = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", path))?;
    let digest = Sha256::digest(path_utf8.as_bytes());
    let mut schema_bytes = [0u8; 8];
    schema_bytes.copy_from_slice(&digest[..8]);
    let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));

    let setup = pg_open_pool(&database_url).await?;
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", schema))
        .execute(&setup)
        .await?;
    drop(setup);

    let pool = pg_open_pool_schematized(&database_url, &schema).await?;

    // Simulate a pre-existing v1+v2 database where data is TEXT.
    PgMigrator::new(&pool, &PLAN_MIGRATIONS[..2]).run().await?;

    // Insert a row with valid JSON stored as TEXT.
    let plan_id = ExecPlanId::new();
    sqlx::query("INSERT INTO exec_plans (id, data) VALUES ($1, $2)")
        .bind(plan_id.as_str())
        .bind(r#"{"id":"test","status":"draft","purpose":"migration contract"}"#)
        .execute(&pool)
        .await?;

    // Apply the remaining migrations (v3: TEXT to JSONB, v4: GIN index).
    PgMigrator::new(&pool, PLAN_MIGRATIONS).run().await?;

    // Assert: data column is now jsonb.
    let (col_type,): (String,) = sqlx::query_as(
        "SELECT data_type FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name   = 'exec_plans'
           AND column_name  = 'data'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        col_type, "jsonb",
        "data column must be jsonb after migration"
    );

    // Assert: GIN index was created.
    let idx: Option<(String,)> = sqlx::query_as(
        "SELECT indexname::text FROM pg_indexes
         WHERE schemaname = current_schema()
           AND tablename  = 'exec_plans'
           AND indexname  = 'idx_exec_plans_data_gin'",
    )
    .fetch_optional(&pool)
    .await?;
    assert!(
        idx.is_some(),
        "idx_exec_plans_data_gin must exist after migration"
    );

    // Assert: pre-existing TEXT row survived the JSONB conversion.
    let (data,): (String,) = sqlx::query_as("SELECT data::text FROM exec_plans WHERE id = $1")
        .bind(plan_id.as_str())
        .fetch_one(&pool)
        .await?;
    let parsed: serde_json::Value = serde_json::from_str(&data)?;
    assert_eq!(parsed["status"], serde_json::json!("draft"));

    Ok(())
}
