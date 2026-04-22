use super::types::{
    PendingCheckpointRow, TaskArtifact, TaskCheckpoint, TaskPrompt, TaskRow, ARTIFACT_MAX_BYTES,
    PROMPT_MAX_BYTES,
};
use super::TaskDb;
use crate::task_runner::TaskState;

impl TaskDb {
    pub async fn write_checkpoint(
        &self,
        task_id: &str,
        triage_output: Option<&str>,
        plan_output: Option<&str>,
        pr_url: Option<&str>,
        last_phase: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO task_checkpoints \
                 (task_id, triage_output, plan_output, pr_url, last_phase, updated_at) \
             VALUES ($1, $2, $3, $4, $5, CURRENT_TIMESTAMP) \
             ON CONFLICT(task_id) DO UPDATE SET \
                 triage_output = COALESCE(excluded.triage_output, task_checkpoints.triage_output), \
                 plan_output   = COALESCE(excluded.plan_output,   task_checkpoints.plan_output), \
                 pr_url        = COALESCE(excluded.pr_url,        task_checkpoints.pr_url), \
                 last_phase    = excluded.last_phase, \
                 updated_at    = excluded.updated_at",
        )
        .bind(task_id)
        .bind(triage_output)
        .bind(plan_output)
        .bind(pr_url)
        .bind(last_phase)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_checkpoint(&self, task_id: &str) -> anyhow::Result<Option<TaskCheckpoint>> {
        let row = sqlx::query_as::<_, TaskCheckpoint>(
            "SELECT task_id, triage_output, plan_output, pr_url, last_phase, \
                    TO_CHAR(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at \
             FROM task_checkpoints WHERE task_id = $1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn insert_artifact(
        &self,
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
            "INSERT INTO task_artifacts (task_id, turn, artifact_type, content) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(task_id)
        .bind(turn as i64)
        .bind(artifact_type)
        .bind(&stored)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_artifacts(&self, task_id: &str) -> anyhow::Result<Vec<TaskArtifact>> {
        let rows = sqlx::query_as::<_, TaskArtifact>(
            "SELECT task_id, turn, artifact_type, content, \
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
             FROM task_artifacts WHERE task_id = $1 ORDER BY id ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn save_task_prompt(
        &self,
        task_id: &str,
        turn: u32,
        phase: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        let stored = if prompt.len() > PROMPT_MAX_BYTES {
            let mut boundary = PROMPT_MAX_BYTES;
            while boundary > 0 && !prompt.is_char_boundary(boundary) {
                boundary -= 1;
            }
            format!("{}\n[TRUNCATED]", &prompt[..boundary])
        } else {
            prompt.to_string()
        };
        sqlx::query(
            "INSERT INTO task_prompts (task_id, turn, phase, prompt) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (task_id, turn, phase) DO UPDATE SET prompt = excluded.prompt",
        )
        .bind(task_id)
        .bind(turn as i64)
        .bind(phase)
        .bind(&stored)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_task_prompts(&self, task_id: &str) -> anyhow::Result<Vec<TaskPrompt>> {
        let rows = sqlx::query_as::<_, TaskPrompt>(
            "SELECT task_id, turn, phase, prompt, \
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
             FROM task_prompts WHERE task_id = $1 ORDER BY turn ASC, id ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn pending_tasks_with_checkpoint(
        &self,
    ) -> anyhow::Result<Vec<(TaskState, TaskCheckpoint)>> {
        let rows = sqlx::query_as::<_, PendingCheckpointRow>(
            "SELECT t.id, t.status, t.turn, t.pr_url, t.rounds, t.error, t.source, \
                    t.external_id, t.parent_id, t.created_at, t.updated_at, t.repo, t.depends_on, \
                    t.project, t.priority, t.phase, t.description, t.request_settings, \
                    c.triage_output, c.plan_output, c.pr_url AS ck_pr_url, \
                    c.last_phase, \
                    TO_CHAR(c.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS ck_updated_at \
             FROM tasks t \
             JOIN task_checkpoints c ON c.task_id = t.id \
             WHERE t.status = 'pending' \
               AND t.pr_url IS NULL \
               AND (c.plan_output IS NOT NULL OR c.triage_output IS NOT NULL)",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut pairs = Vec::with_capacity(rows.len());
        for row in rows {
            let task_id = row.id.clone();
            let task_row = TaskRow {
                id: row.id,
                status: row.status,
                turn: row.turn,
                pr_url: row.pr_url,
                rounds: row.rounds,
                error: row.error,
                source: row.source,
                external_id: row.external_id,
                parent_id: row.parent_id,
                created_at: row.created_at,
                updated_at: row.updated_at,
                repo: row.repo,
                depends_on: row.depends_on,
                project: row.project,
                priority: row.priority,
                phase: row.phase,
                description: row.description,
                request_settings: row.request_settings,
            };
            let task_state = match task_row.try_into_task_state() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        "skipping malformed pending checkpoint task: {e}"
                    );
                    continue;
                }
            };
            let checkpoint = TaskCheckpoint {
                task_id,
                triage_output: row.triage_output,
                plan_output: row.plan_output,
                pr_url: row.ck_pr_url,
                last_phase: row.last_phase,
                updated_at: row.ck_updated_at,
            };
            pairs.push((task_state, checkpoint));
        }
        Ok(pairs)
    }
}
