use super::*;

impl WorkflowRuntimeStore {
    /// Store the prompt bytes a `prompt_ref` names.
    ///
    /// The ref is content-addressed by its producer, so the same ref must
    /// always name the same bytes. Writing it once and proving equality on
    /// repeat keeps that true: a replay is idempotent, and a second writer
    /// claiming an existing ref for different bytes is refused instead of
    /// silently rebinding the ref every earlier record already points at
    /// (GH-1865).
    pub async fn insert_prompt_payload(
        &self,
        prompt_ref: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        insert_prompt_payload_tx(&mut tx, prompt_ref, prompt).await?;
        tx.commit().await?;
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

/// A `prompt_ref` that already names different bytes than the ones being
/// written. The stored payload is what every existing record resolves to, so
/// the write is refused rather than rebinding the ref.
#[derive(Debug, Clone)]
pub struct PromptPayloadIntegrityError {
    pub prompt_ref: String,
    pub stored_len: usize,
    pub attempted_len: usize,
}

impl std::fmt::Display for PromptPayloadIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prompt ref `{}` already stores different prompt bytes ({} stored vs {} attempted); \
             refusing to rebind a content-addressed ref",
            self.prompt_ref, self.stored_len, self.attempted_len
        )
    }
}

impl std::error::Error for PromptPayloadIntegrityError {}

/// Insert prompt bytes once.
///
/// Equal bytes under an existing ref are an idempotent replay; different bytes
/// are a [`PromptPayloadIntegrityError`].
///
/// This checks the ref against what is stored, not against a hash of the
/// prompt: a `prompt_ref` digests submission identity alongside the prompt, so
/// only its producer can recompute it. The store's guarantee is narrower and
/// still sufficient — one ref never resolves to two different payloads.
pub(in crate::runtime) async fn insert_prompt_payload_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    prompt_ref: &str,
    prompt: &str,
) -> anyhow::Result<()> {
    if prompt_ref.trim().is_empty() {
        anyhow::bail!("workflow prompt payload prompt_ref must not be empty");
    }
    let inserted = sqlx::query(
        "INSERT INTO workflow_prompt_payloads (prompt_ref, prompt)
         VALUES ($1, $2)
         ON CONFLICT (prompt_ref) DO NOTHING",
    )
    .bind(prompt_ref)
    .bind(prompt)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(());
    }

    let stored: Option<(String,)> =
        sqlx::query_as("SELECT prompt FROM workflow_prompt_payloads WHERE prompt_ref = $1")
            .bind(prompt_ref)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((stored_prompt,)) = stored else {
        anyhow::bail!("prompt ref `{prompt_ref}` conflicted on insert but could not be read back");
    };
    if stored_prompt == prompt {
        return Ok(());
    }
    Err(PromptPayloadIntegrityError {
        prompt_ref: prompt_ref.to_string(),
        stored_len: stored_prompt.len(),
        attempted_len: prompt.len(),
    }
    .into())
}
