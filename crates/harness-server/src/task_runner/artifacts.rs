use super::{TaskId, TaskStore};
use crate::observation_compression::RawObservationSink;
use std::sync::Arc;

pub(crate) struct TaskArtifactSink {
    store: Arc<TaskStore>,
}

impl TaskArtifactSink {
    pub(crate) fn new(store: Arc<TaskStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl RawObservationSink for TaskArtifactSink {
    async fn persist_raw(
        &self,
        task_id: &TaskId,
        turn: u32,
        artifact_type: &str,
        raw: &str,
    ) -> anyhow::Result<()> {
        self.store
            .insert_artifact_strict(task_id, turn, artifact_type, raw)
            .await
    }
}

impl TaskStore {
    /// Persist a complete raw observation or fail before TaskDb can truncate it.
    pub(crate) async fn insert_artifact_strict(
        &self,
        task_id: &TaskId,
        turn: u32,
        artifact_type: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        ensure_lossless_artifact_size(content)?;
        self.db
            .insert_artifact(&task_id.0, turn, artifact_type, content)
            .await
    }
}

fn ensure_lossless_artifact_size(content: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        content.len() <= crate::task_db::ARTIFACT_MAX_BYTES,
        "raw artifact is {} bytes; maximum lossless artifact size is {} bytes",
        content.len(),
        crate::task_db::ARTIFACT_MAX_BYTES
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_insert_rejects_content_that_task_db_would_truncate() {
        let content = "x".repeat(crate::task_db::ARTIFACT_MAX_BYTES + 1);
        let error = ensure_lossless_artifact_size(&content).unwrap_err();

        assert!(error.to_string().contains("maximum lossless artifact size"));
    }
}
