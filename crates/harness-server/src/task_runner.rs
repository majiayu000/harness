use dashmap::DashMap;
use harness_core::{prompts, AgentRequest, CodeAgent};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string()[..8].to_string())
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateTaskRequest {
    /// The only required field — natural language task description.
    /// Can be anything: "fix majiayu000/harness#4", an issue URL,
    /// or a plain sentence like "给 harness 加单元测试".
    pub prompt: String,
    #[serde(default = "default_project")]
    pub project: PathBuf,
    #[serde(default = "default_wait")]
    pub wait_secs: u64,
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
}

fn default_project() -> PathBuf {
    PathBuf::from(".")
}
fn default_wait() -> u64 {
    120
}
fn default_max_rounds() -> u32 {
    5
}

pub type TaskStore = Arc<DashMap<String, TaskState>>;

pub fn new_task_store() -> TaskStore {
    Arc::new(DashMap::new())
}

pub fn spawn_task(
    store: TaskStore,
    agent: Arc<dyn CodeAgent>,
    req: CreateTaskRequest,
) -> TaskId {
    let task_id = TaskId::new();
    let state = TaskState::new(task_id.clone());
    store.insert(task_id.0.clone(), state);

    let id = task_id.clone();
    let store = store.clone();

    tokio::spawn(async move {
        if let Err(e) = run_task(&store, &id, agent.as_ref(), &req).await {
            if let Some(mut s) = store.get_mut(&id.0) {
                s.status = TaskStatus::Failed;
                s.error = Some(e.to_string());
            }
        }
    });

    task_id
}

async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    req: &CreateTaskRequest,
) -> anyhow::Result<()> {
    // Turn 1: implement
    update_status(store, task_id, TaskStatus::Implementing, 1);

    let resp = agent
        .execute(AgentRequest {
            prompt: prompts::implement_from_prompt(&req.prompt),
            project_root: req.project.clone(),
            ..Default::default()
        })
        .await?;

    let pr_url = prompts::parse_pr_url(&resp.output);
    let pr_number = pr_url.as_ref().and_then(|u| prompts::extract_pr_number(u));

    if let Some(mut s) = store.get_mut(&task_id.0) {
        s.pr_url = pr_url.clone();
        s.rounds.push(RoundResult {
            turn: 1,
            action: "implement".into(),
            result: if pr_url.is_some() {
                "pr_created".into()
            } else {
                "implemented".into()
            },
        });
    }

    // If no PR was created or no review loop needed, we're done
    let pr_num = match pr_number {
        Some(n) => n,
        None => {
            update_status(store, task_id, TaskStatus::Done, 1);
            return Ok(());
        }
    };

    // Review loop: Turn 2..N
    for round in 2..=(req.max_rounds + 1) {
        update_status(store, task_id, TaskStatus::Waiting, round);
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round);

        let resp = agent
            .execute(AgentRequest {
                prompt: prompts::review_prompt(None, pr_num),
                project_root: req.project.clone(),
                ..Default::default()
            })
            .await?;

        let lgtm = prompts::is_lgtm(&resp.output);

        if let Some(mut s) = store.get_mut(&task_id.0) {
            s.rounds.push(RoundResult {
                turn: round,
                action: "review".into(),
                result: if lgtm { "lgtm".into() } else { "fixed".into() },
            });
        }

        if lgtm {
            update_status(store, task_id, TaskStatus::Done, round);
            return Ok(());
        }
    }

    update_status(store, task_id, TaskStatus::Done, req.max_rounds + 1);
    Ok(())
}

fn update_status(store: &TaskStore, task_id: &TaskId, status: TaskStatus, turn: u32) {
    if let Some(mut s) = store.get_mut(&task_id.0) {
        s.status = status;
        s.turn = turn;
    }
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
