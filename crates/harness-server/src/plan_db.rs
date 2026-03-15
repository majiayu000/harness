use crate::db::Db;
use harness_core::{ExecPlanId, ExecPlanStatus};
use harness_exec::ExecPlan;
use std::path::Path;

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

    pub async fn delete(&self, id: &ExecPlanId) -> anyhow::Result<bool> {
        self.inner.delete(id.as_str()).await
    }

    /// Return all plans whose status matches `status`.
    pub async fn list_by_status(&self, status: ExecPlanStatus) -> anyhow::Result<Vec<ExecPlan>> {
        let status_str = serde_json::to_value(status)?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let sql =
            "SELECT data FROM exec_plans WHERE json_extract(data, '$.status') = ? ORDER BY created_at DESC";
        let rows: Vec<(String,)> = sqlx::query_as(sql)
            .bind(&status_str)
            .fetch_all(self.inner.pool())
            .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Return all plans whose purpose contains `query` (case-insensitive substring match).
    pub async fn search_by_name(&self, query: &str) -> anyhow::Result<Vec<ExecPlan>> {
        let pattern = format!("%{}%", query);
        let sql =
            "SELECT data FROM exec_plans WHERE json_extract(data, '$.purpose') LIKE ? ORDER BY created_at DESC";
        let rows: Vec<(String,)> = sqlx::query_as(sql)
            .bind(&pattern)
            .fetch_all(self.inner.pool())
            .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    /// Import ExecPlan Markdown files from `dir` into the database.
    ///
    /// Files are only imported when no plan with the same ID already exists,
    /// so re-running migration on subsequent startups is safe (idempotent).
    pub async fn migrate_from_markdown_dir(&self, dir: &Path) -> anyhow::Result<usize> {
        if !dir.is_dir() {
            return Ok(0);
        }
        let mut imported = 0usize;
        let entries = std::fs::read_dir(dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "plan migration: failed to read file: {e}");
                    continue;
                }
            };
            let plan = match ExecPlan::from_markdown(&content) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %path.display(), "plan migration: failed to parse markdown: {e}");
                    continue;
                }
            };
            // Skip if already persisted.
            if self.inner.get(plan.id.as_str()).await?.is_some() {
                continue;
            }
            if let Err(e) = self.upsert(&plan).await {
                tracing::warn!(path = %path.display(), "plan migration: failed to upsert: {e}");
                continue;
            }
            tracing::info!(id = plan.id.as_str(), purpose = %plan.purpose, "plan migration: imported from markdown");
            imported += 1;
        }
        Ok(imported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::ExecPlanStatus;

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

    #[tokio::test]
    async fn plan_db_delete_removes_plan() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;

        let plan = ExecPlan::from_spec("# Delete me", &dir.path().to_path_buf())?;
        db.upsert(&plan).await?;
        assert!(db.delete(&plan.id).await?);
        assert!(db.get(&plan.id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn plan_db_delete_missing_returns_false() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;
        assert!(!db.delete(&ExecPlanId::new()).await?);
        Ok(())
    }

    #[tokio::test]
    async fn list_by_status_filters_correctly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;

        let draft = ExecPlan::from_spec("# Draft plan", &dir.path().to_path_buf())?;
        let mut active = ExecPlan::from_spec("# Active plan", &dir.path().to_path_buf())?;
        active.activate();
        let mut completed = ExecPlan::from_spec("# Completed plan", &dir.path().to_path_buf())?;
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
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;

        let auth = ExecPlan::from_spec("# Implement authentication", &dir.path().to_path_buf())?;
        let deploy = ExecPlan::from_spec("# Deploy to production", &dir.path().to_path_buf())?;

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
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;

        // Write a minimal ExecPlan markdown file.
        let plan = ExecPlan::from_spec("# Migration test plan", &dir.path().to_path_buf())?;
        let md = plan.to_markdown();
        std::fs::write(dir.path().join("plan1.md"), &md)?;

        let count = db.migrate_from_markdown_dir(dir.path()).await?;
        assert_eq!(count, 1);

        let loaded = db.get(&plan.id).await?.expect("migrated plan should exist");
        assert_eq!(loaded.purpose, plan.purpose);

        // Second run should not re-import.
        let count2 = db.migrate_from_markdown_dir(dir.path()).await?;
        assert_eq!(count2, 0);
        Ok(())
    }

    #[tokio::test]
    async fn migrate_from_markdown_dir_skips_nonexistent_dir() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = PlanDb::open(&dir.path().join("plans.db")).await?;
        let count = db
            .migrate_from_markdown_dir(&dir.path().join("nonexistent"))
            .await?;
        assert_eq!(count, 0);
        Ok(())
    }
}
