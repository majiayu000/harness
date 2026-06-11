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
        sqlx::query("UPDATE tasks SET scheduler_state = $1 WHERE id = $2")
            .bind(raw_scheduler_state)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
