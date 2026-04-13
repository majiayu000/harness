use super::schema::ARTIFACT_MAX_BYTES;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;

/// A single persisted artifact captured from agent output during task execution.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TaskArtifact {
    pub task_id: String,
    pub turn: i64,
    pub artifact_type: String,
    pub content: String,
    pub created_at: String,
}

/// Persist a single artifact captured from agent output.
///
/// Content larger than [`ARTIFACT_MAX_BYTES`] is truncated to avoid
/// unbounded database growth without requiring an external compression
/// dependency.
pub(super) async fn insert_artifact(
    pool: &SqlitePool,
    task_id: &str,
    turn: u32,
    artifact_type: &str,
    content: &str,
) -> anyhow::Result<()> {
    let stored = if content.len() > ARTIFACT_MAX_BYTES {
        let mut boundary = ARTIFACT_MAX_BYTES;
        while boundary > 0 && !content.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!(
            "{}\n[truncated: {} bytes total]",
            &content[..boundary],
            content.len()
        )
    } else {
        content.to_string()
    };

    sqlx::query(
        "INSERT INTO task_artifacts (task_id, turn, artifact_type, content)
         VALUES (?, ?, ?, ?)",
    )
    .bind(task_id)
    .bind(turn as i64)
    .bind(artifact_type)
    .bind(&stored)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return all artifacts for a task ordered by insertion time.
pub(super) async fn list_artifacts(
    pool: &SqlitePool,
    task_id: &str,
) -> anyhow::Result<Vec<TaskArtifact>> {
    let rows = sqlx::query_as::<_, TaskArtifact>(
        "SELECT task_id, turn, artifact_type, content, created_at
         FROM task_artifacts WHERE task_id = ? ORDER BY id ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
