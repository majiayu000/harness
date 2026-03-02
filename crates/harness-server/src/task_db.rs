use crate::task_runner::{TaskId, TaskState, TaskStatus};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

pub struct TaskDb {
    pool: SqlitePool,
}

impl TaskDb {
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
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                status TEXT NOT NULL DEFAULT 'pending',
                turn INTEGER NOT NULL DEFAULT 0,
                pr_url TEXT,
                rounds TEXT NOT NULL DEFAULT '[]',
                error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let status = status_to_str(&state.status);
        sqlx::query(
            "INSERT INTO tasks (id, status, turn, pr_url, rounds, error)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&state.id.0)
        .bind(status)
        .bind(state.turn as i64)
        .bind(&state.pr_url)
        .bind(&rounds_json)
        .bind(&state.error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update(&self, state: &TaskState) -> anyhow::Result<()> {
        let rounds_json = serde_json::to_string(&state.rounds)?;
        let status = status_to_str(&state.status);
        sqlx::query(
            "UPDATE tasks SET status = ?, turn = ?, pr_url = ?, rounds = ?, error = ?,
                    updated_at = datetime('now')
             WHERE id = ?",
        )
        .bind(status)
        .bind(state.turn as i64)
        .bind(&state.pr_url)
        .bind(&rounds_json)
        .bind(&state.error)
        .bind(&state.id.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<TaskState>> {
        let row = sqlx::query_as::<_, TaskRow>("SELECT * FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.into_task_state()))
    }

    pub async fn list(&self) -> anyhow::Result<Vec<TaskState>> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT * FROM tasks ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.into_task_state()).collect())
    }
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: String,
    status: String,
    turn: i64,
    pr_url: Option<String>,
    rounds: String,
    error: Option<String>,
    #[allow(dead_code)]
    created_at: String,
    #[allow(dead_code)]
    updated_at: String,
}

impl TaskRow {
    fn into_task_state(self) -> TaskState {
        TaskState {
            id: TaskId(self.id),
            status: str_to_status(&self.status),
            turn: self.turn as u32,
            pr_url: self.pr_url,
            rounds: serde_json::from_str(&self.rounds).unwrap_or_default(),
            error: self.error,
        }
    }
}

fn status_to_str(s: &TaskStatus) -> &'static str {
    match s {
        TaskStatus::Pending => "pending",
        TaskStatus::Implementing => "implementing",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Reviewing => "reviewing",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
    }
}

fn str_to_status(s: &str) -> TaskStatus {
    match s {
        "pending" => TaskStatus::Pending,
        "implementing" => TaskStatus::Implementing,
        "waiting" => TaskStatus::Waiting,
        "reviewing" => TaskStatus::Reviewing,
        "done" => TaskStatus::Done,
        "failed" => TaskStatus::Failed,
        _ => TaskStatus::Failed,
    }
}
