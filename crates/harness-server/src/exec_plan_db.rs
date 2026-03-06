use harness_core::ExecPlanId;
use harness_exec::ExecPlan;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

pub struct ExecPlanDb {
    pool: SqlitePool,
}

impl ExecPlanDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS exec_plans (
                id TEXT PRIMARY KEY,
                plan_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert(&self, plan: &ExecPlan) -> anyhow::Result<()> {
        let plan_json = serde_json::to_string(plan)?;
        sqlx::query(
            "INSERT INTO exec_plans (id, plan_json, created_at, updated_at)
             VALUES (?, ?, datetime('now'), datetime('now'))
             ON CONFLICT(id) DO UPDATE
             SET plan_json = excluded.plan_json, updated_at = datetime('now')",
        )
        .bind(plan.id.as_str())
        .bind(&plan_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, plan_id: &ExecPlanId) -> anyhow::Result<Option<ExecPlan>> {
        let stored =
            sqlx::query_scalar::<_, String>("SELECT plan_json FROM exec_plans WHERE id = ?")
                .bind(plan_id.as_str())
                .fetch_optional(&self.pool)
                .await?;

        stored
            .map(|plan_json| {
                serde_json::from_str::<ExecPlan>(&plan_json).map_err(anyhow::Error::from)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn exec_plan_db_migrates_table_on_open() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("exec_plans.db");
        let db = ExecPlanDb::open(&db_path).await?;

        let table_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name='exec_plans'",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(table_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn exec_plan_db_roundtrip_update() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("exec_plans.db");
        let db = ExecPlanDb::open(&db_path).await?;

        let mut plan = ExecPlan::from_spec("# Persist plan", Path::new("/tmp"))?;
        db.upsert(&plan).await?;

        let loaded = db.get(&plan.id).await?.expect("expected persisted plan");
        assert_eq!(loaded.id, plan.id);
        assert_eq!(loaded.status, harness_core::ExecPlanStatus::Draft);

        plan.activate();
        db.upsert(&plan).await?;
        let updated = db.get(&plan.id).await?.expect("expected updated plan");
        assert_eq!(updated.status, harness_core::ExecPlanStatus::Active);
        Ok(())
    }

    #[tokio::test]
    async fn exec_plan_db_survives_reopen() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("exec_plans.db");

        let persisted_plan_id = {
            let db = ExecPlanDb::open(&db_path).await?;
            let plan = ExecPlan::from_spec("# Restart recovery", Path::new("/tmp"))?;
            let plan_id = plan.id.clone();
            db.upsert(&plan).await?;
            plan_id
        };

        let reopened_db = ExecPlanDb::open(&db_path).await?;
        let recovered = reopened_db
            .get(&persisted_plan_id)
            .await?
            .expect("expected plan after reopen");
        assert_eq!(recovered.id, persisted_plan_id);
        assert_eq!(recovered.purpose, "Restart recovery");
        Ok(())
    }
}
