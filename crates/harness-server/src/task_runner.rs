use crate::task_db::TaskDb;
use dashmap::DashMap;
use harness_core::{prompts, AgentRequest, CodeAgent, ContextItem, Decision, Event, SessionId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
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

    pub fn count(&self) -> usize {
        self.cache.len()
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
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    req: CreateTaskRequest,
) -> TaskId {
    let task_id = TaskId::new();
    let state = TaskState::new(task_id.clone());
    store.insert(&state).await;

    let id = task_id.clone();
    // Clone handles for the watcher spawn; originals move into the inner spawn.
    let store_watcher = store.clone();
    let id_watcher = id.clone();

    // Inner task: runs the actual work, returns Result so the watcher can detect errors.
    let handle = tokio::spawn(async move {
        let project = match req.project.clone() {
            Some(p) => p,
            None => tokio::task::spawn_blocking(detect_main_worktree)
                .await
                .unwrap_or_else(|_| PathBuf::from(".")),
        };
        run_task(&store, &id, agent.as_ref(), skills, events, &req, project).await
    });

    // Watcher: awaits the inner JoinHandle to propagate both errors and panics to the store.
    // Without this, a panic inside the inner task would leave the task stuck in a non-terminal state.
    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                mutate_and_persist(&store_watcher, &id_watcher, |s| {
                    s.status = TaskStatus::Failed;
                    s.error = Some(e.to_string());
                })
                .await;
            }
            Err(join_err) => {
                tracing::error!("task {id_watcher:?} panicked or was cancelled: {join_err}");
                mutate_and_persist(&store_watcher, &id_watcher, |s| {
                    s.status = TaskStatus::Failed;
                    s.error = Some(format!("task failed unexpectedly: {join_err}"));
                })
                .await;
            }
        }
    });

    task_id
}

async fn run_task(
    store: &TaskStore,
    task_id: &TaskId,
    agent: &dyn CodeAgent,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
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

    let skill_items: Vec<ContextItem> = {
        let guard = skills.read().await;
        guard
            .list()
            .iter()
            .map(|s| ContextItem::Skill {
                id: s.id.to_string(),
                content: s.content.clone(),
            })
            .collect()
    };

    let turn_timeout = Duration::from_secs(req.turn_timeout_secs);
    let resp = tokio::time::timeout(
        turn_timeout,
        agent.execute(AgentRequest {
            prompt: first_prompt,
            project_root: project.clone(),
            context: skill_items.clone(),
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
    let last_review_round = req.max_rounds.saturating_add(1);
    for round in 2..=last_review_round {
        update_status(store, task_id, TaskStatus::Waiting, round).await;
        sleep(Duration::from_secs(req.wait_secs)).await;

        update_status(store, task_id, TaskStatus::Reviewing, round).await;

        let resp = tokio::time::timeout(
            turn_timeout,
            agent.execute(AgentRequest {
                prompt: prompts::review_prompt(req.issue, pr_num, round),
                project_root: project.clone(),
                context: skill_items.clone(),
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

        // Log pr_review event for observability and GC signal detection.
        let mut ev = Event::new(
            SessionId::new(),
            "pr_review",
            "task_runner",
            if lgtm { Decision::Complete } else { Decision::Warn },
        );
        ev.detail = Some(format!("pr={pr_num}"));
        ev.reason = Some(if lgtm {
            format!("round {round}: lgtm")
        } else {
            format!("round {round}: fixed")
        });
        if let Err(e) = events.log(&ev) {
            tracing::warn!("failed to log pr_review event: {e}");
        }

        if lgtm {
            update_status(store, task_id, TaskStatus::Done, round).await;
            return Ok(());
        }
    }

    // Reached max rounds without LGTM
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.turn = req.max_rounds.saturating_add(1);
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
    use async_trait::async_trait;
    use harness_core::{AgentResponse, Capability, StreamItem, TokenUsage};

    #[test]
    fn test_task_state_new() {
        let id = TaskId::new();
        let state = TaskState::new(id);
        assert!(matches!(state.status, TaskStatus::Pending));
        assert_eq!(state.turn, 0);
        assert!(state.pr_url.is_none());
    }

    /// A mock agent that captures the context from the first AgentRequest it receives.
    struct CapturingAgent {
        captured: tokio::sync::Mutex<Vec<ContextItem>>,
    }

    impl CapturingAgent {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                captured: tokio::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl harness_core::CodeAgent for CapturingAgent {
        fn name(&self) -> &str {
            "capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
            let mut guard = self.captured.lock().await;
            if guard.is_empty() {
                *guard = req.context.clone();
            }
            Ok(AgentResponse {
                output: String::new(),
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage { input_tokens: 0, output_tokens: 0, total_tokens: 0, cost_usd: 0.0 },
                model: "mock".into(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn skills_are_injected_into_agent_context() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        // Build a SkillStore with one skill
        let mut skill_store = harness_skills::SkillStore::new();
        skill_store.create("test-skill".to_string(), "do something useful".to_string());
        let skills = Arc::new(RwLock::new(skill_store));

        let agent = CapturingAgent::new();
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("test task".into()),
            issue: None,
            pr: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: 0,
            turn_timeout_secs: 30,
        };

        let events = Arc::new(harness_observe::EventStore::new(dir.path())?);
        spawn_task(store, agent_clone, skills, events, req).await;

        // Allow the spawned task to run
        tokio::time::sleep(Duration::from_millis(200)).await;

        let captured = agent.captured.lock().await;
        assert!(!captured.is_empty(), "expected skills to be injected into AgentRequest.context");
        assert!(
            captured.iter().any(|item| matches!(item, ContextItem::Skill { .. })),
            "expected at least one ContextItem::Skill"
        );
        Ok(())
    }
}
