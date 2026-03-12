use crate::db::{Db, DbEntity};
use harness_core::ExecPlanId;
use harness_exec::ExecPlan;
use std::path::Path;

const CREATE_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS plans (
    id         TEXT PRIMARY KEY,
    data       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
)";

impl DbEntity for ExecPlan {
    fn table_name() -> &'static str {
        "plans"
    }

    fn id(&self) -> &str {
        self.id.as_str()
    }

    fn create_table_sql() -> &'static str {
        CREATE_TABLE_SQL
    }
}

pub struct PlanDb {
    inner: Db<ExecPlan>,
}

impl PlanDb {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Db::open(path).await?,
        })
    }

    pub async fn upsert(&self, plan: &ExecPlan) -> anyhow::Result<()> {
        self.inner.upsert(plan).await
    }

    pub async fn get(&self, id: &ExecPlanId) -> anyhow::Result<Option<ExecPlan>> {
        self.inner.get(id.as_str()).await
    }

    pub async fn list(&self) -> anyhow::Result<Vec<ExecPlan>> {
        self.inner.list().await
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
