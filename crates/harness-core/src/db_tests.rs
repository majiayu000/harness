use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Note {
    id: String,
    body: String,
}

impl Note {
    fn new(id: &str, body: &str) -> Self {
        Self {
            id: id.to_string(),
            body: body.to_string(),
        }
    }
}

impl DbEntity for Note {
    fn table_name() -> &'static str {
        "notes"
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn create_table_sql() -> &'static str {
        "CREATE TABLE IF NOT EXISTS notes (
            id         TEXT PRIMARY KEY,
            data       TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"
    }
}

#[tokio::test]
async fn upsert_and_get_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

    let note = Note::new("n1", "hello");
    db.upsert(&note).await?;

    let loaded = db
        .get("n1")
        .await?
        .ok_or_else(|| anyhow::anyhow!("note should exist"))?;
    assert_eq!(loaded, note);
    Ok(())
}

#[tokio::test]
async fn get_returns_none_for_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

    assert!(db.get("missing").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn upsert_overwrites_existing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

    db.upsert(&Note::new("n1", "original")).await?;
    db.upsert(&Note::new("n1", "updated")).await?;

    let loaded = db
        .get("n1")
        .await?
        .ok_or_else(|| anyhow::anyhow!("note should exist after upsert"))?;
    assert_eq!(loaded.body, "updated");

    let all = db.list().await?;
    assert_eq!(all.len(), 1, "upsert should not duplicate rows");
    Ok(())
}

#[tokio::test]
async fn list_returns_all_entities() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

    db.upsert(&Note::new("n1", "a")).await?;
    db.upsert(&Note::new("n2", "b")).await?;
    db.upsert(&Note::new("n3", "c")).await?;

    let all = db.list().await?;
    assert_eq!(all.len(), 3);
    Ok(())
}

#[tokio::test]
async fn delete_returns_true_when_found() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

    db.upsert(&Note::new("n1", "bye")).await?;
    assert!(db.delete("n1").await?);
    assert!(db.get("n1").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn delete_returns_false_when_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db = Db::<Note>::open(&dir.path().join("notes.db")).await?;

    assert!(!db.delete("nonexistent").await?);
    Ok(())
}

#[tokio::test]
async fn survives_reopen() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("notes.db");

    {
        let db = Db::<Note>::open(&db_path).await?;
        db.upsert(&Note::new("persistent", "content")).await?;
    }

    let db = Db::<Note>::open(&db_path).await?;
    let loaded = db
        .get("persistent")
        .await?
        .ok_or_else(|| anyhow::anyhow!("note should survive reopen"))?;
    assert_eq!(loaded.body, "content");
    Ok(())
}

// --- Migrator tests ---

static SIMPLE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create items table",
        sql: "CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY, value TEXT NOT NULL)",
    },
    Migration {
        version: 2,
        description: "add tag column",
        sql: "ALTER TABLE items ADD COLUMN tag TEXT",
    },
];

async fn applied_versions(pool: &SqlitePool) -> anyhow::Result<Vec<i64>> {
    Ok(
        sqlx::query_as::<_, (i64,)>("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|(version,)| version)
            .collect(),
    )
}

async fn table_columns(pool: &SqlitePool, table: &str) -> anyhow::Result<Vec<String>> {
    let pragma = format!("PRAGMA table_info({table})");
    Ok(
        sqlx::query_as::<_, (i64, String, String, i64, Option<String>, i64)>(&pragma)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|(_, name, _, _, _, _)| name)
            .collect(),
    )
}

#[tokio::test]
async fn migrator_applies_pending_migrations() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;

    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

    // Both versions should be recorded.
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 1);
    assert_eq!(rows[1].0, 2);
    Ok(())
}

#[tokio::test]
async fn migrator_is_idempotent_on_rerun() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;

    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
    // Running again must not fail or duplicate rows.
    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await?;
    assert_eq!(rows.len(), 2);
    Ok(())
}

#[tokio::test]
async fn migrator_tolerates_duplicate_column_on_alter_table() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;

    // Run only version 1 first (creates the table).
    Migrator::new(&pool, &SIMPLE_MIGRATIONS[..1]).run().await?;

    // Manually add the column that migration v2 would add.
    sqlx::query("ALTER TABLE items ADD COLUMN tag TEXT")
        .execute(&pool)
        .await?;

    // Running v2 now must not fail even though the column already exists.
    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
    assert_eq!(applied_versions(&pool).await?, vec![1, 2]);
    Ok(())
}

#[tokio::test]
async fn failing_multi_statement_migration_is_not_recorded() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;
    let migrations = [
        Migration {
            version: 1,
            description: "create items table",
            sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
        },
        Migration {
            version: 2,
            description: "broken migration",
            sql: "ALTER TABLE items ADD COLUMN tag TEXT; SELECT nope FROM missing_table",
        },
    ];

    let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();

    assert!(
        err.to_string()
            .contains("migration v2 'broken migration' failed"),
        "unexpected error: {err:#}"
    );
    assert_eq!(applied_versions(&pool).await?, vec![1]);
    Ok(())
}

#[tokio::test]
async fn failing_multi_statement_migration_does_not_partially_persist() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;
    let migrations = [
        Migration {
            version: 1,
            description: "create items table",
            sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
        },
        Migration {
            version: 2,
            description: "broken migration",
            sql: "ALTER TABLE items ADD COLUMN tag TEXT; SELECT nope FROM missing_table",
        },
    ];

    let _ = Migrator::new(&pool, &migrations).run().await;

    assert_eq!(applied_versions(&pool).await?, vec![1]);
    assert_eq!(table_columns(&pool, "items").await?, vec!["id"]);
    Ok(())
}

#[tokio::test]
async fn migration_error_reports_failed_statement_index() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;
    let migrations = [
        Migration {
            version: 1,
            description: "create items table",
            sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
        },
        Migration {
            version: 2,
            description: "broken migration",
            sql: "ALTER TABLE items ADD COLUMN tag TEXT; SELECT nope FROM missing_table",
        },
    ];

    let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
    let message = err.to_string();

    assert!(
        message.contains("migration v2 'broken migration' failed at statement 2"),
        "unexpected error: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn pragma_foreign_keys_migration_runs_outside_transaction() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;
    let migrations = [
        Migration {
            version: 1,
            description: "create items table",
            sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
        },
        Migration {
            version: 2,
            description: "pragma migration",
            sql: "PRAGMA foreign_keys = OFF; SELECT nope FROM missing_table",
        },
    ];

    let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
    let message = err.to_string();

    assert!(
        message.contains("outside transaction"),
        "unexpected error: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn multiline_create_table_migration_stays_transactional() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;
    let migrations = [Migration {
        version: 1,
        description: "broken multiline create table",
        sql: "CREATE\nTABLE items (id TEXT PRIMARY KEY); SELECT nope FROM missing_table",
    }];

    let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
    let message = err.to_string();

    assert!(
        !message.contains("outside transaction"),
        "unexpected error: {message}"
    );
    assert_eq!(applied_versions(&pool).await?, Vec::<i64>::new());
    assert_eq!(table_columns(&pool, "items").await?, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn pragma_defer_foreign_keys_migration_runs_outside_transaction() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pool = open_pool(&dir.path().join("mig.db")).await?;
    let migrations = [
        Migration {
            version: 1,
            description: "create items table",
            sql: "CREATE TABLE items (id TEXT PRIMARY KEY)",
        },
        Migration {
            version: 2,
            description: "pragma defer migration",
            sql: "PRAGMA defer_foreign_keys = ON; SELECT nope FROM missing_table",
        },
    ];

    let err = Migrator::new(&pool, &migrations).run().await.unwrap_err();
    let message = err.to_string();

    assert!(
        message.contains("outside transaction"),
        "unexpected error: {message}"
    );
    Ok(())
}

#[test]
fn migration_statements_keep_trigger_body_intact() {
    let statements = migration_statements(
        "CREATE TABLE items (id INTEGER PRIMARY KEY);\n\
         CREATE TRIGGER items_touch AFTER INSERT ON items BEGIN\n\
         UPDATE items SET id = NEW.id;\n\
         INSERT INTO items_log DEFAULT VALUES;\n\
         END;",
    );

    assert_eq!(statements.len(), 2);
    assert_eq!(
        statements[0].sql,
        "CREATE TABLE items (id INTEGER PRIMARY KEY)"
    );
    assert!(
        statements[1]
            .sql
            .starts_with("CREATE TRIGGER items_touch AFTER INSERT ON items BEGIN"),
        "unexpected trigger statement: {}",
        statements[1].sql
    );
    assert!(
        statements[1].sql.ends_with("END"),
        "unexpected trigger statement: {}",
        statements[1].sql
    );
}

#[test]
fn migration_statements_split_explicit_transaction_control() {
    let statements = migration_statements(
        "BEGIN;\n\
         CREATE TABLE items (id INTEGER PRIMARY KEY);\n\
         INSERT INTO items VALUES (1);\n\
         COMMIT;",
    );

    assert_eq!(statements.len(), 4);
    assert_eq!(statements[0].sql, "BEGIN");
    assert_eq!(
        statements[1].sql,
        "CREATE TABLE items (id INTEGER PRIMARY KEY)"
    );
    assert_eq!(statements[2].sql, "INSERT INTO items VALUES (1)");
    assert_eq!(statements[3].sql, "COMMIT");
}

#[test]
fn migration_statements_keep_case_end_inside_trigger_body() {
    let statements = migration_statements(
        "CREATE TRIGGER items_touch AFTER INSERT ON items BEGIN\n\
         UPDATE items\n\
         SET id = CASE WHEN NEW.id IS NULL THEN OLD.id ELSE NEW.id END;\n\
         INSERT INTO items_log DEFAULT VALUES;\n\
         END;",
    );

    assert_eq!(statements.len(), 1);
    assert!(
        statements[0]
            .sql
            .contains("CASE WHEN NEW.id IS NULL THEN OLD.id ELSE NEW.id END;"),
        "unexpected trigger statement: {}",
        statements[0].sql
    );
    assert!(
        statements[0]
            .sql
            .contains("INSERT INTO items_log DEFAULT VALUES;"),
        "unexpected trigger statement: {}",
        statements[0].sql
    );
    assert!(
        statements[0].sql.ends_with("END"),
        "unexpected trigger statement: {}",
        statements[0].sql
    );
}

#[tokio::test]
async fn pragma_style_migration_keeps_connection_local_state_on_one_connection(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("mig.db").display());
    let pool = SqlitePoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await?;
    let migrations = [Migration {
        version: 1,
        description: "temp table migration",
        sql: "CREATE TEMP TABLE migration_temp (id INTEGER); \
              INSERT INTO migration_temp VALUES (1); \
              SELECT * FROM migration_temp",
    }];

    Migrator::new(&pool, &migrations).run().await?;
    assert_eq!(applied_versions(&pool).await?, vec![1]);
    Ok(())
}
