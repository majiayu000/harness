use super::TaskDb;

impl TaskDb {
    /// Overwrite the `scheduler_state` column with arbitrary raw text.
    ///
    /// Used by integration tests to simulate a corrupted DB payload without going
    /// through the normal serialization path. The name suffix `_for_test` signals
    /// that production code must never call this.
    #[doc(hidden)]
    pub async fn overwrite_scheduler_state_for_test(
        &self,
        task_id: &str,
        raw_scheduler_state: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET scheduler_state = $1 WHERE store_key = $2 AND id = $3")
            .bind(raw_scheduler_state)
            .bind(&self.store_key)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[doc(hidden)]
    pub async fn overwrite_updated_at_for_test(
        &self,
        task_id: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET updated_at = $1 WHERE store_key = $2 AND id = $3")
            .bind(updated_at)
            .bind(&self.store_key)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
