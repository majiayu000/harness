use super::*;

impl WorkflowRuntimeStore {
    pub async fn upsert_prompt_payload(
        &self,
        prompt_ref: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        if prompt_ref.trim().is_empty() {
            anyhow::bail!("workflow prompt payload prompt_ref must not be empty");
        }
        sqlx::query(
            "INSERT INTO workflow_prompt_payloads (prompt_ref, prompt)
             VALUES ($1, $2)
             ON CONFLICT (prompt_ref) DO UPDATE SET
                prompt = EXCLUDED.prompt,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(prompt_ref)
        .bind(prompt)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn get_prompt_payload(&self, prompt_ref: &str) -> anyhow::Result<Option<String>> {
        if prompt_ref.trim().is_empty() {
            return Ok(None);
        }
        let row: Option<(String,)> =
            sqlx::query_as("SELECT prompt FROM workflow_prompt_payloads WHERE prompt_ref = $1")
                .bind(prompt_ref)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(prompt,)| prompt))
    }

    pub async fn delete_prompt_payload(&self, prompt_ref: &str) -> anyhow::Result<()> {
        if prompt_ref.trim().is_empty() {
            return Ok(());
        }
        sqlx::query("DELETE FROM workflow_prompt_payloads WHERE prompt_ref = $1")
            .bind(prompt_ref)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
