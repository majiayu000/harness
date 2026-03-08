use crate::task_db::TaskDb;
use dashmap::DashMap;
use harness_core::{CodeAgent, Decision, Event, SessionId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

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
    AgentReview,
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
    /// Explicit agent name; if omitted, uses the default agent.
    pub agent: Option<String>,
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

impl Default for CreateTaskRequest {
    fn default() -> Self {
        Self {
            prompt: None,
            issue: None,
            pr: None,
            agent: None,
            project: None,
            wait_secs: default_wait(),
            max_rounds: default_max_rounds(),
            turn_timeout_secs: default_turn_timeout(),
        }
    }
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
            tracing::warn!(
                "detect_main_worktree: could not detect git worktree root, falling back to '.'"
            );
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

fn describe_detect_main_worktree_join_error(join_err: &tokio::task::JoinError) -> String {
    if join_err.is_panic() {
        format!("detect_main_worktree panicked: {join_err}")
    } else if join_err.is_cancelled() {
        format!("detect_main_worktree was cancelled: {join_err}")
    } else {
        format!("detect_main_worktree failed: {join_err}")
    }
}

async fn resolve_project_root_with(
    requested_project: Option<PathBuf>,
    detect_worktree: impl FnOnce() -> PathBuf + Send + 'static,
) -> anyhow::Result<PathBuf> {
    match requested_project {
        Some(project) => Ok(project),
        None => tokio::task::spawn_blocking(detect_worktree)
            .await
            .map_err(|join_err| {
                let reason = describe_detect_main_worktree_join_error(&join_err);
                tracing::error!("{reason}");
                anyhow::anyhow!("{reason}")
            }),
    }
}

fn log_task_failure_event(events: &harness_observe::EventStore, task_id: &TaskId, reason: &str) {
    let mut event = Event::new(
        SessionId::new(),
        "task_failure",
        "task_runner",
        Decision::Block,
    );
    event.reason = Some(reason.to_string());
    event.detail = Some(format!("task_id={}", task_id.0));
    if let Err(e) = events.log(&event) {
        tracing::warn!("failed to log task_failure event for {task_id:?}: {e}");
    }
}

async fn record_task_failure(
    store: &TaskStore,
    events: &harness_observe::EventStore,
    task_id: &TaskId,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason);
    mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.error = Some(reason);
    })
    .await;
}

/// In-memory cache + SQLite persistence.
pub struct TaskStore {
    pub(crate) cache: DashMap<TaskId, TaskState>,
    db: TaskDb,
    persist_locks: DashMap<TaskId, Arc<Mutex<()>>>,
}

impl TaskStore {
    pub async fn open(db_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open(db_path).await?;
        let cache = DashMap::new();
        let persist_locks = DashMap::new();
        for task in db.list().await? {
            persist_locks.insert(task.id.clone(), Arc::new(Mutex::new(())));
            cache.insert(task.id.clone(), task);
        }
        Ok(Arc::new(Self {
            cache,
            db,
            persist_locks,
        }))
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

    pub(crate) async fn insert(&self, state: &TaskState) {
        self.persist_locks
            .entry(state.id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())));
        self.cache.insert(state.id.clone(), state.clone());
        if let Err(e) = self.db.insert(state).await {
            tracing::error!("task_db insert failed: {e}");
        }
    }

    pub(crate) async fn persist(&self, id: &TaskId) {
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let snapshot = self.cache.get(id).map(|state| state.value().clone());
        if let Some(state) = snapshot {
            if let Err(e) = self.db.update(&state).await {
                tracing::error!("task_db update failed: {e}");
            }
        }
    }
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    review_config: harness_core::AgentReviewConfig,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
) -> TaskId {
    spawn_task_with_worktree_detector(
        store,
        agent,
        reviewer,
        review_config,
        skills,
        events,
        interceptors,
        req,
        detect_main_worktree,
    )
    .await
}

async fn spawn_task_with_worktree_detector<F>(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    review_config: harness_core::AgentReviewConfig,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    detect_worktree: F,
) -> TaskId
where
    F: Fn() -> PathBuf + Send + Sync + 'static,
{
    let task_id = TaskId::new();
    let state = TaskState::new(task_id.clone());
    store.insert(&state).await;

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let interceptors = Arc::new(interceptors);
    let detect_worktree = Arc::new(detect_worktree);

    let handle = tokio::spawn(async move {
        let detect_worktree = detect_worktree.clone();
        let raw_project =
            resolve_project_root_with(req.project.clone(), move || detect_worktree()).await?;
        let project = crate::handlers::validate_project_root(&raw_project)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        crate::task_executor::run_task(
            &store,
            &id,
            agent.as_ref(),
            reviewer.as_deref(),
            &review_config,
            skills,
            events,
            interceptors,
            &req,
            project,
        )
        .await
    });

    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, e.to_string())
                    .await;
            }
            Err(join_err) => {
                tracing::error!("task {id_watcher:?} panicked or was cancelled: {join_err}");
                let reason = format!("task failed unexpectedly: {join_err}");
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, reason).await;
            }
        }
    });

    task_id
}

pub(crate) async fn update_status(
    store: &TaskStore,
    task_id: &TaskId,
    status: TaskStatus,
    turn: u32,
) {
    mutate_and_persist(store, task_id, |s| {
        s.status = status;
        s.turn = turn;
    })
    .await;
}

/// Mutate a task in the cache then persist to SQLite.
pub(crate) async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) {
    let _ = mutate_and_persist_with(store, id, |state| {
        f(state);
    })
    .await;
}

/// Mutate a task, compute a return value from the same in-lock snapshot, then persist.
pub(crate) async fn mutate_and_persist_with<R>(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState) -> R,
) -> Option<R> {
    let result = if let Some(mut entry) = store.cache.get_mut(id) {
        let result = f(entry.value_mut());
        Some(result)
    } else {
        None
    };

    store.persist(id).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::{
        AgentRequest, AgentResponse, Capability, ContextItem, EventFilters, StreamItem, TokenUsage,
    };
    use tokio::time::Duration;

    #[test]
    fn test_task_state_new() {
        let id = TaskId::new();
        let state = TaskState::new(id);
        assert!(matches!(state.status, TaskStatus::Pending));
        assert_eq!(state.turn, 0);
        assert!(state.pr_url.is_none());
    }

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
                token_usage: TokenUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    total_tokens: 0,
                    cost_usd: 0.0,
                },
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
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut skill_store = harness_skills::SkillStore::new();
        skill_store.create("test-skill".to_string(), "do something useful".to_string());
        let skills = Arc::new(RwLock::new(skill_store));

        let agent = CapturingAgent::new();
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("test task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: 0,
            turn_timeout_secs: 30,
        };

        let events = Arc::new(harness_observe::EventStore::new(dir.path())?);
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let captured = agent.captured.lock().await;
        assert!(
            !captured.is_empty(),
            "expected skills to be injected into AgentRequest.context"
        );
        assert!(
            captured
                .iter()
                .any(|item| matches!(item, ContextItem::Skill { .. })),
            "expected at least one ContextItem::Skill"
        );
        Ok(())
    }

    struct BlockingInterceptor;

    #[async_trait]
    impl harness_core::interceptor::TurnInterceptor for BlockingInterceptor {
        fn name(&self) -> &str {
            "blocking-test"
        }

        async fn pre_execute(
            &self,
            _req: &AgentRequest,
        ) -> harness_core::interceptor::InterceptResult {
            harness_core::interceptor::InterceptResult::block("test block")
        }
    }

    #[tokio::test]
    async fn blocking_interceptor_fails_task() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::EventStore::new(dir.path())?);

        let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
            vec![Arc::new(BlockingInterceptor)];

        let req = CreateTaskRequest {
            prompt: Some("blocked task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: 0,
            turn_timeout_secs: 30,
        };

        let task_id = spawn_task(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events,
            interceptors,
            req,
        )
        .await;

        // Allow async task to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = store.get(&task_id).expect("task must exist");
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed, got {:?}",
            state.status
        );
        assert!(
            state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Blocked by interceptor"),
            "error message should mention blocked: {:?}",
            state.error
        );
        Ok(())
    }

    #[tokio::test]
    async fn mutate_and_persist_with_counts_waiting_entries_in_single_snapshot(
    ) -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let task_id = TaskId::new();
        let task_state = TaskState::new(task_id.clone());
        store.insert(&task_state).await;

        let mut handles = Vec::new();
        let round = 2u32;
        let workers = 8u32;

        for _ in 0..workers {
            let store = store.clone();
            let task_id = task_id.clone();
            handles.push(tokio::spawn(async move {
                mutate_and_persist_with(store.as_ref(), &task_id, |state| {
                    state.rounds.push(RoundResult {
                        turn: round,
                        action: "review".into(),
                        result: "waiting".into(),
                    });
                    state
                        .rounds
                        .iter()
                        .filter(|result| result.turn == round && result.result == "waiting")
                        .count() as u32
                })
                .await
                .expect("task state should exist")
            }));
        }

        let mut observed_counts = Vec::new();
        for handle in handles {
            observed_counts.push(handle.await?);
        }
        observed_counts.sort_unstable();

        assert_eq!(observed_counts, (1..=workers).collect::<Vec<u32>>());

        let state = store.get(&task_id).expect("task state should exist");
        let waiting_entries = state
            .rounds
            .iter()
            .filter(|result| result.turn == round && result.result == "waiting")
            .count() as u32;
        assert_eq!(waiting_entries, workers);

        let persisted = store
            .db
            .get(&task_id.0)
            .await?
            .expect("persisted task state should exist");
        assert_eq!(persisted.rounds.len(), state.rounds.len());
        Ok(())
    }

    #[tokio::test]
    async fn spawn_blocking_panic_surfaces_error_and_event() -> anyhow::Result<()> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = tempfile::Builder::new()
            .prefix("harness-test-")
            .tempdir_in(&home)?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::EventStore::new(dir.path())?);

        let req = CreateTaskRequest {
            prompt: Some("panic path".into()),
            issue: None,
            pr: None,
            agent: None,
            project: None,
            wait_secs: 0,
            max_rounds: 0,
            turn_timeout_secs: 30,
        };

        let task_id = spawn_task_with_worktree_detector(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events.clone(),
            vec![],
            req,
            || -> PathBuf {
                panic!("forced detect_main_worktree panic");
            },
        )
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = store.get(&task_id).expect("task must exist");
        assert!(
            matches!(state.status, TaskStatus::Failed),
            "expected Failed, got {:?}",
            state.status
        );
        let error = state.error.unwrap_or_default();
        assert!(
            error.contains("detect_main_worktree panicked"),
            "expected panic reason in task error, got: {error}"
        );
        assert!(
            error.contains("forced detect_main_worktree panic"),
            "expected panic payload in task error, got: {error}"
        );

        let expected_detail = format!("task_id={}", task_id.0);
        let failure_events = events.query(&EventFilters {
            hook: Some("task_failure".to_string()),
            ..Default::default()
        })?;
        assert!(
            failure_events.iter().any(|event| {
                event.detail.as_deref() == Some(expected_detail.as_str())
                    && event
                        .reason
                        .as_deref()
                        .unwrap_or_default()
                        .contains("forced detect_main_worktree panic")
            }),
            "expected task_failure event containing panic payload, got: {:?}",
            failure_events
        );
        Ok(())
    }
}
