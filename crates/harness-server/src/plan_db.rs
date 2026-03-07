use harness_core::ExecPlanId;
use harness_exec::ExecPlan;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

pub struct PlanDb {
    pool: SqlitePool,
}

impl PlanDb {
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
            "CREATE TABLE IF NOT EXISTS plans (
                id TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert(&self, plan: &ExecPlan) -> anyhow::Result<()> {
        let data = serde_json::to_string(plan)?;
        sqlx::query(
            "INSERT INTO plans (id, data) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data,
                updated_at = datetime('now')",
        )
        .bind(plan.id.as_str())
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &ExecPlanId) -> anyhow::Result<Option<ExecPlan>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT data FROM plans WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some((data,)) => Ok(Some(serde_json::from_str(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<ExecPlan>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT data FROM plans ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plan_db_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;

        let plan = ExecPlan::from_spec("Test plan purpose", &dir.path().to_path_buf())?;
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
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;
        assert!(db.get(&ExecPlanId::new()).await?.is_none());
        Ok(())
    }
}
