use crate::task_db::TaskDb;
use dashmap::DashMap;
use harness_core::{prompts, AgentRequest, CodeAgent};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Implementing,
    Waiting,
    Reviewing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundResult {
    pub turn: u32,
    pub action: String,
    pub result: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub id: TaskId,
    pub status: TaskStatus,
    pub turn: u32,
    pub pr_url: Option<String>,
    pub rounds: Vec<RoundResult>,
    pub error: Option<String>,
}

/// Lightweight task summary returned by the list endpoint (excludes `rounds` history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub status: TaskStatus,
    pub turn: u32,
    pub pr_url: Option<String>,
    pub error: Option<String>,
}

impl TaskState {
    fn new(id: TaskId) -> Self {
        Self {
            id,
            status: TaskStatus::Pending,
            turn: 0,
            pr_url: None,
            rounds: Vec::new(),
            error: None,
        }
    }

    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id.clone(),
            status: self.status.clone(),
            turn: self.turn,
            pr_url: self.pr_url.clone(),
            error: self.error.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTaskRequest {
    /// Free-text task description (prompt, issue URL, etc.).
    pub prompt: Option<String>,
    /// GitHub issue number to implement from.
    pub issue: Option<u64>,
    /// GitHub PR number to review/fix.
    pub pr: Option<u64>,
    /// Project root; defaults to the main git worktree resolved at task spawn time.
    pub project: Option<PathBuf>,
    #[serde(default = "default_wait")]
    pub wait_secs: u64,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    /// Per-turn timeout in seconds; defaults to 600 (10 min).
    #[serde(default = "default_turn_timeout")]
    pub turn_timeout_secs: u64,
}

/// Detect the main git worktree root using a blocking subprocess call.
/// Must be called via `tokio::task::spawn_blocking` in async contexts.
fn detect_main_worktree() -> PathBuf {
    std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|line| line.strip_prefix("worktree "))
                .map(|p| PathBuf::from(p.trim()))
        })
        .unwrap_or_else(|| {
            tracing::warn!("detect_main_worktree: could not detect git worktree root, falling back to '.'");
            PathBuf::from(".")
        })
}

fn default_wait() -> u64 {
    120
}
fn default_max_rounds() -> u32 {
    5
}
fn default_turn_timeout() -> u64 {
    600
}

/// In-memory cache + SQLite persistence.
pub struct TaskStore {
    cache: DashMap<TaskId, TaskState>,
    db: TaskDb,
}

impl TaskStore {
    pub async fn open(db_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open(db_path).await?;
        let cache = DashMap::new();
        // Load existing tasks into cache
        for task in db.list().await? {
            cache.insert(task.id.clone(), task);
        }
        Ok(Arc::new(Self { cache, db }))
    }

    pub fn get(&self, id: &TaskId) -> Option<TaskState> {
        self.cache.get(id).map(|r| r.value().clone())
    }

    pub fn list_all(&self) -> Vec<TaskState> {
        self.cache.iter().map(|e| e.value().clone()).collect()
    }

    async fn insert(&self, state: &TaskState) {
        self.cache.insert(state.id.clone(), state.clone());
        if let Err(e) = self.db.insert(state).await {
            tracing::error!("task_db insert failed: {e}");
        }
    }

    async fn persist(&self, id: &TaskId) {
        if let Some(state) = self.cache.get(id) {
            if let Err(e) = self.db.update(state.value()).await {
                tracing::error!("task_db update failed: {e}");
            }
        }
    }
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    req: CreateTaskRequest,
) -> TaskId {
    let task_id = TaskId::new();
    let state = TaskState::new(task_id.clone());
    store.insert(&state).await;

    let id = task_id.clone();
    let store = store.clone();

    tokio::spawn(async move {
        let project = match req.project.clone() {
            Some(p) => p,
            None => tokio::task::spawn_blocking(detect_main_worktree)
                .await
                .unwrap_or_else(|_| PathBuf::from(".")),
        };
        if let Err(e) = run_task(&store, &id, agent.as_ref(), &req, project).await {
            mutate_and_persist(&store, &id, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(e.to_string());
            })
            .await;
        }
    });

    task_id
}

async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
    project: PathBuf,
) -> anyhow::Result<()> {
    // Turn 1: implement
    update_status(store, task_id, TaskStatus::Implementing, 1).await;

    let first_prompt = if let Some(issue) = req.issue {
        prompts::implement_from_issue(issue)
    } else if let Some(pr) = req.pr {
        prompts::check_existing_pr(pr)
    } else {
        prompts::implement_from_prompt(req.prompt.as_deref().unwrap_or_default())
    };

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);
    let resp = tokio::time::timeout(
        turn_timeout,
        agent.execute(AgentRequest {
            prompt: first_prompt,
            project_root: project.clone(),
            ..Default::default()
        }),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Turn 1 timed out after {}s", req.turn_timeout_secs))?
    ?;

    let pr_url = prompts::parse_pr_url(&resp.output);
    let pr_number = pr_url
        .as_ref()
        .and_then(|u| prompts::extract_pr_number(u))
        .or(req.pr); // fallback: use req.pr if agent didn't output PR_URL

    mutate_and_persist(store, task_id, |s| {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_url.is_some() || req.pr.is_some() {
                "pr_created".into()
            } else {
                "implemented".into()
            },
        });
    })
    .await;

    let pr_num = match pr_number {
        Some(n) => n,
        None => {
            update_status(store, task_id, TaskStatus::Done, 1).await;
            return Ok(());
        }
    };

    // Review loop: Turn 2..N
    let last_review_round = req.max_rounds + 1;
    for round in 2..=last_review_round {
        update_status(store, task_id, TaskStatus::Waiting, round).await;
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round).await;

        let is_last_round = round == last_review_round;
        let resp = tokio::time::timeout(
            turn_timeout,
            agent.execute(AgentRequest {
                prompt: prompts::review_prompt(req.issue, pr_num, round, is_last_round),
                project_root: project.clone(),
                ..Default::default()
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Turn {round} timed out after {}s", req.turn_timeout_secs))?
        ?;

        let lgtm = prompts::is_lgtm(&resp.output);

        mutate_and_persist(store, task_id, |s| {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if lgtm { "lgtm".into() } else { "fixed".into() },
            });
        })
        .await;

        if lgtm {
            update_status(store, task_id, TaskStatus::Done, round).await;
            return Ok(());
        }
    }

    // Reached max rounds without LGTM
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = req.max_rounds + 1;
        s.error = Some(format!(
            "Task did not receive LGTM after {} review rounds.",
            req.max_rounds
        ));
    })
    .await;
    Ok(())
}

async fn update_status(store: &TaskStore, task_id: &TaskId, status: TaskStatus, turn: u32) {
    mutate_and_persist(store, task_id, |s| {
        s.status = status;
        s.turn = turn;
    })
    .await;
}

/// Mutate a task in the cache then persist to SQLite.
async fn mutate_and_persist(store: &TaskStore, id: &TaskId, f: impl FnOnce(&mut TaskState)) {
    if let Some(mut entry) = store.cache.get_mut(id) {
        f(entry.value_mut());
    }
    store.persist(id).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_state_new() {
        let id = TaskId::new();
        let state = TaskState::new(id);
        assert!(matches!(state.status, TaskStatus::Pending));
        assert_eq!(state.turn, 0);
        assert!(state.pr_url.is_none());
    }
}
