use super::*;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
use std::future::Future;

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
            created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )"
    }
}

fn run_db_test<F, Fut>(test: F) -> anyhow::Result<()>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let _lock = crate::test_support::process_env_lock();
    runtime.block_on(test())
}

macro_rules! db_test {
    ($name:ident, $body:block) => {
        #[test]
        fn $name() -> anyhow::Result<()> {
            run_db_test(|| async $body)
        }
    };
}

fn db_path(name: &str) -> anyhow::Result<std::path::PathBuf> {
    let dir = tempfile::tempdir()?;
    Ok(dir.path().join(name))
}

async fn applied_versions(pool: &PgPool) -> anyhow::Result<Vec<i64>> {
    Ok(
        sqlx::query_as::<_, (i64,)>("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|(version,)| version)
            .collect(),
    )
}

async fn table_columns(pool: &PgPool, table: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_as::<_, (String,)>(
        "SELECT column_name
         FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = $1
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(name,)| name)
    .collect())
}

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

db_test!(upsert_and_get_roundtrip, {
    let db = Db::<Note>::open_legacy_path_schema(&db_path("notes.db")?).await?;

    let note = Note::new("n1", "hello");
    db.upsert(&note).await?;

    let loaded = db
        .get("n1")
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected note"))?;
    assert_eq!(loaded, note);
    Ok(())
});

db_test!(get_returns_none_for_missing, {
    let db = Db::<Note>::open_legacy_path_schema(&db_path("notes.db")?).await?;

    assert!(db.get("missing").await?.is_none());
    Ok(())
});

db_test!(upsert_overwrites_existing_and_list_does_not_duplicate, {
    let db = Db::<Note>::open_legacy_path_schema(&db_path("notes.db")?).await?;

    db.upsert(&Note::new("n1", "original")).await?;
    db.upsert(&Note::new("n1", "updated")).await?;

    let loaded = db
        .get("n1")
        .await?
        .ok_or_else(|| anyhow::anyhow!("expected note"))?;
    assert_eq!(loaded.body, "updated");

    let list = db.list().await?;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].body, "updated");
    Ok(())
});

db_test!(list_returns_all_entities, {
    let db = Db::<Note>::open_legacy_path_schema(&db_path("notes.db")?).await?;

    db.upsert(&Note::new("n1", "a")).await?;
    db.upsert(&Note::new("n2", "b")).await?;

    let mut ids: Vec<String> = db.list().await?.into_iter().map(|n| n.id).collect();
    ids.sort();
    assert_eq!(ids, vec!["n1", "n2"]);
    Ok(())
});

db_test!(delete_roundtrip, {
    let db = Db::<Note>::open_legacy_path_schema(&db_path("notes.db")?).await?;

    db.upsert(&Note::new("n1", "bye")).await?;
    assert!(db.delete("n1").await?);
    assert!(db.get("n1").await?.is_none());
    assert!(!db.delete("n1").await?);
    Ok(())
});

db_test!(survives_reopen_for_same_path, {
    let db_path = db_path("notes.db")?;

    {
        let db = Db::<Note>::open_legacy_path_schema(&db_path).await?;
        db.upsert(&Note::new("n1", "persisted")).await?;
    }

    {
        let db = Db::<Note>::open_legacy_path_schema(&db_path).await?;
        let loaded = db
            .get("n1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("expected persisted note"))?;
        assert_eq!(loaded.body, "persisted");
    }
    Ok(())
});

db_test!(different_paths_are_schema_isolated, {
    let db_a = Db::<Note>::open_legacy_path_schema(&db_path("notes-a.db")?).await?;
    let db_b = Db::<Note>::open_legacy_path_schema(&db_path("notes-b.db")?).await?;

    db_a.upsert(&Note::new("shared-id", "alpha")).await?;
    db_b.upsert(&Note::new("shared-id", "beta")).await?;

    let loaded_a = db_a
        .get("shared-id")
        .await?
        .ok_or_else(|| anyhow::anyhow!("row should exist in first schema"))?;
    let loaded_b = db_b
        .get("shared-id")
        .await?
        .ok_or_else(|| anyhow::anyhow!("row should exist in second schema"))?;

    assert_eq!(loaded_a.body, "alpha");
    assert_eq!(loaded_b.body, "beta");
    assert_eq!(db_a.list().await?.len(), 1);
    assert_eq!(db_b.list().await?.len(), 1);
    Ok(())
});

db_test!(db_legacy_path_schema_open_uses_deterministic_schema_name, {
    let path = db_path("notes.db")?;
    let expected_schema = pg_schema_for_path(&path)?;
    let db = Db::<Note>::open_legacy_path_schema(&path).await?;

    let (actual_schema,): (String,) = sqlx::query_as("SELECT current_schema()")
        .fetch_one(db.pool())
        .await?;
    assert_eq!(actual_schema, expected_schema);
    Ok(())
});

db_test!(migrator_applies_pending_migrations, {
    let pool = open_legacy_path_schema_pool(&db_path("mig.db")?).await?;

    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

    assert_eq!(applied_versions(&pool).await?, vec![1, 2]);
    assert_eq!(
        table_columns(&pool, "items").await?,
        vec!["id", "value", "tag"]
    );
    Ok(())
});

db_test!(migrator_is_idempotent_on_rerun, {
    let pool = open_legacy_path_schema_pool(&db_path("mig.db")?).await?;

    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;
    Migrator::new(&pool, SIMPLE_MIGRATIONS).run().await?;

    assert_eq!(applied_versions(&pool).await?, vec![1, 2]);
    Ok(())
});

db_test!(migrator_serializes_concurrent_runs_for_same_schema, {
    let path = db_path("mig-concurrent.db")?;
    let pool_a = open_legacy_path_schema_pool(&path).await?;
    let pool_b = open_legacy_path_schema_pool(&path).await?;
    let migrations = [
        Migration {
            version: 1,
            description: "create concurrent items table",
            sql: "SELECT pg_sleep(0.05);
                  CREATE TABLE IF NOT EXISTS concurrent_items (
                      id TEXT PRIMARY KEY,
                      value TEXT NOT NULL
                  )",
        },
        Migration {
            version: 2,
            description: "add concurrent tag column",
            sql: "ALTER TABLE concurrent_items ADD COLUMN IF NOT EXISTS tag TEXT",
        },
    ];

    let migrator_a = Migrator::new(&pool_a, &migrations);
    let migrator_b = Migrator::new(&pool_b, &migrations);
    let run_a = migrator_a.run();
    let run_b = migrator_b.run();
    let (result_a, result_b) = tokio::join!(run_a, run_b);
    result_a?;
    result_b?;

    assert_eq!(applied_versions(&pool_a).await?, vec![1, 2]);
    assert_eq!(
        table_columns(&pool_a, "concurrent_items").await?,
        vec!["id", "value", "tag"]
    );
    Ok(())
});

db_test!(migrator_tolerates_duplicate_column_on_alter_table, {
    let pool = open_legacy_path_schema_pool(&db_path("mig.db")?).await?;
    sqlx::query("CREATE TABLE items (id TEXT PRIMARY KEY, value TEXT NOT NULL, tag TEXT)")
        .execute(&pool)
        .await?;

    let migrations = [Migration {
        version: 1,
        description: "add existing tag column",
        sql: "ALTER TABLE items ADD COLUMN tag TEXT",
    }];

    Migrator::new(&pool, &migrations).run().await?;

    assert_eq!(applied_versions(&pool).await?, vec![1]);
    assert_eq!(
        table_columns(&pool, "items").await?,
        vec!["id", "value", "tag"]
    );
    Ok(())
});

db_test!(failing_migration_is_not_recorded, {
    let pool = open_legacy_path_schema_pool(&db_path("mig.db")?).await?;
    let migrations = [Migration {
        version: 1,
        description: "bad migration",
        sql: "CREATE TABLE bad_items (id TEXT PRIMARY KEY); SELECT * FROM missing_table",
    }];

    let err = Migrator::new(&pool, &migrations)
        .run()
        .await
        .expect_err("migration should fail");
    assert!(
        err.to_string().contains("bad migration"),
        "unexpected error: {err}"
    );

    let (exists,): (Option<String>,) = sqlx::query_as("SELECT to_regclass('bad_items')::TEXT")
        .fetch_one(&pool)
        .await?;
    assert_eq!(exists, None);
    assert_eq!(applied_versions(&pool).await?, Vec::<i64>::new());
    Ok(())
});
