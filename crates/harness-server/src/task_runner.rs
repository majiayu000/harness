use crate::task_db::TaskDb;
use dashmap::DashMap;
pub use harness_core::TaskId;
use harness_core::{CodeAgent, Decision, Event, SessionId, StreamItem};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

/// Broadcast channel capacity for per-task stream events.
/// When the buffer is full, the oldest events are dropped for lagging receivers.
const TASK_STREAM_CAPACITY: usize = 512;

/// Async callback invoked when a task reaches a terminal state (Done/Failed).
/// Receives a snapshot of the final `TaskState`. Implemented in the intake layer
/// to call `IntakeSource::on_task_complete` without creating circular dependencies.
pub type CompletionCallback =
    Arc<dyn Fn(TaskState) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

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

impl AsRef<str> for TaskStatus {
    fn as_ref(&self) -> &str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Implementing => "implementing",
            TaskStatus::AgentReview => "agent_review",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Reviewing => "reviewing",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
        }
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(TaskStatus::Pending),
            "implementing" => Ok(TaskStatus::Implementing),
            "agent_review" => Ok(TaskStatus::AgentReview),
            "waiting" => Ok(TaskStatus::Waiting),
            "reviewing" => Ok(TaskStatus::Reviewing),
            "done" => Ok(TaskStatus::Done),
            "failed" => Ok(TaskStatus::Failed),
            _ => anyhow::bail!("unknown task status `{s}`"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundResult {
    pub turn: u32,
    pub action: String,
    pub result: String,
    /// Raw output from the reviewer agent, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub id: TaskId,
    pub status: TaskStatus,
    pub turn: u32,
    pub pr_url: Option<String>,
    pub rounds: Vec<RoundResult>,
    pub error: Option<String>,
    /// Intake source name that created this task (e.g. "github", "feishu").
    /// Persisted so the association survives server restart.
    pub source: Option<String>,
    /// Source-specific identifier for the originating issue/message.
    /// Used by `CompletionCallback` to call `IntakeSource::on_task_complete`.
    pub external_id: Option<String>,
    /// Parent task ID when this task is a subtask spawned via parallel dispatch.
    /// Persisted so parent-child relationships survive server restart.
    pub parent_id: Option<TaskId>,
    /// IDs of subtasks spawned by this task during parallel dispatch.
    /// Populated at runtime; not persisted (use `TaskStore::list_children` after restart).
    #[serde(default)]
    pub subtask_ids: Vec<TaskId>,
}

/// Lightweight task summary returned by the list endpoint (excludes `rounds` history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub status: TaskStatus,
    pub turn: u32,
    pub pr_url: Option<String>,
    pub error: Option<String>,
    /// Intake source name (e.g. "github", "feishu", "dashboard"). None for manual tasks.
    pub source: Option<String>,
    /// Parent task ID when this is a subtask.
    pub parent_id: Option<TaskId>,
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
            source: None,
            external_id: None,
            parent_id: None,
            subtask_ids: Vec::new(),
        }
    }

    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id.clone(),
            status: self.status.clone(),
            turn: self.turn,
            pr_url: self.pr_url.clone(),
            error: self.error.clone(),
            source: self.source.clone(),
            parent_id: self.parent_id.clone(),
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
    /// Per-turn timeout in seconds; defaults to 3600 (1 hour).
    #[serde(default = "default_turn_timeout")]
    pub turn_timeout_secs: u64,
    /// Maximum spend budget for the agent in USD; None means unlimited.
    #[serde(default)]
    pub max_budget_usd: Option<f64>,
    /// Base delay in milliseconds for the first validation retry; subsequent retries double the
    /// delay up to `retry_max_backoff_ms`. Default: 10 000 ms (10 s).
    #[serde(default = "default_retry_base_backoff_ms")]
    pub retry_base_backoff_ms: u64,
    /// Maximum backoff cap in milliseconds for validation retries. Default: 300 000 ms (5 min).
    #[serde(default = "default_retry_max_backoff_ms")]
    pub retry_max_backoff_ms: u64,
    /// Seconds of silence from the agent stream before declaring a stall; defaults to 300.
    /// Overrides the global `concurrency.stall_timeout_secs` for this task.
    #[serde(default = "default_stall_timeout")]
    pub stall_timeout_secs: u64,
    /// Intake source name (e.g. "github", "feishu", "periodic_review"). None for manual tasks.
    #[serde(default)]
    pub source: Option<String>,
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
            max_budget_usd: None,
            retry_base_backoff_ms: default_retry_base_backoff_ms(),
            retry_max_backoff_ms: default_retry_max_backoff_ms(),
            stall_timeout_secs: default_stall_timeout(),
            source: None,
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
fn default_retry_base_backoff_ms() -> u64 {
    10_000
}
fn default_retry_max_backoff_ms() -> u64 {
    300_000
}
fn default_turn_timeout() -> u64 {
    // 1 hour: parallel subtasks and complex agent turns on large codebases
    // regularly exceed the previous 10-minute default when running CI checks,
    // building dependencies from source, or iterating on review feedback.
    3600
}
fn default_stall_timeout() -> u64 {
    300
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

async fn log_task_failure_event(
    events: &harness_observe::EventStore,
    task_id: &TaskId,
    reason: &str,
) {
    let mut event = Event::new(
        SessionId::new(),
        "task_failure",
        "task_runner",
        Decision::Block,
    );
    event.reason = Some(reason.to_string());
    event.detail = Some(format!("task_id={}", task_id.0));
    if let Err(e) = events.log(&event).await {
        tracing::warn!("failed to log task_failure event for {task_id:?}: {e}");
    }
}

async fn record_task_failure(
    store: &TaskStore,
    events: &harness_observe::EventStore,
    task_id: &TaskId,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
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
    /// Per-task broadcast channels for real-time stream forwarding to SSE clients.
    stream_txs: DashMap<TaskId, broadcast::Sender<StreamItem>>,
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
            stream_txs: DashMap::new(),
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

    /// Return all cached tasks whose `parent_id` matches the given ID.
    /// Reconstructs the child list from in-memory state; does not require
    /// `subtask_ids` to be persisted on the parent.
    pub fn list_children(&self, parent_id: &TaskId) -> Vec<TaskState> {
        self.cache
            .iter()
            .filter(|e| e.value().parent_id.as_ref() == Some(parent_id))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Register a broadcast channel for a task's stream. Call once before task execution starts.
    pub(crate) fn register_task_stream(&self, id: &TaskId) {
        let (tx, _rx) = broadcast::channel(TASK_STREAM_CAPACITY);
        self.stream_txs.insert(id.clone(), tx);
    }

    /// Subscribe to a task's active stream. Returns `None` if no stream is registered.
    pub fn subscribe_task_stream(&self, id: &TaskId) -> Option<broadcast::Receiver<StreamItem>> {
        self.stream_txs.get(id).map(|tx| tx.subscribe())
    }

    /// Publish a [`StreamItem`] to all current subscribers of a task stream.
    /// No-op when no stream is registered. Dropping the send result is intentional:
    /// `SendError` only occurs when there are no active receivers, which is normal
    /// when no SSE client is connected (backpressure: oldest events dropped on lag).
    pub(crate) fn publish_stream_item(&self, id: &TaskId, item: StreamItem) {
        if let Some(tx) = self.stream_txs.get(id) {
            if let Err(e) = tx.send(item) {
                tracing::trace!(task_id = %id.0, "stream publish dropped (no receivers): {e}");
            }
        }
    }

    /// Remove the task's stream channel after execution completes.
    pub(crate) fn close_task_stream(&self, id: &TaskId) {
        self.stream_txs.remove(id);
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
    server_config: std::sync::Arc<harness_core::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
) -> TaskId {
    spawn_task_with_worktree_detector(
        store,
        agent,
        reviewer,
        server_config,
        skills,
        events,
        interceptors,
        req,
        detect_main_worktree,
        workspace_mgr,
        permit,
        completion_callback,
    )
    .await
}

async fn spawn_task_with_worktree_detector<F>(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::SkillStore>>,
    events: Arc<harness_observe::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    detect_worktree: F,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
) -> TaskId
where
    F: Fn() -> PathBuf + Send + Sync + 'static,
{
    let task_id = TaskId::new();
    let mut state = TaskState::new(task_id.clone());
    state.source = req.source.clone();
    store.insert(&state).await;
    // Register stream channel before spawning so SSE clients can subscribe immediately.
    store.register_task_stream(&task_id);

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let interceptors = Arc::new(interceptors);
    let detect_worktree = Arc::new(detect_worktree);

    let handle = tokio::spawn(async move {
        // Hold the concurrency permit for the task's lifetime.
        // Dropped automatically when this future completes (including on panic).
        let _permit = permit;
        let detect_worktree = detect_worktree.clone();
        let raw_project =
            resolve_project_root_with(req.project.clone(), move || detect_worktree()).await?;
        let project_root = crate::handlers::validate_project_root(&raw_project)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // Parallel dispatch for Complex+ prompt-only tasks when workspace isolation is active.
        // Issue and PR tasks have their own structured prompt flow and are not decomposed.
        let classification = crate::complexity_router::classify(
            req.prompt.as_deref().unwrap_or_default(),
            req.issue,
            req.pr,
        );
        let is_complex = matches!(
            classification.complexity,
            harness_core::TaskComplexity::Complex | harness_core::TaskComplexity::Critical
        );
        if req.issue.is_none() && req.pr.is_none() && is_complex {
            if let Some(ref wmgr) = workspace_mgr {
                let subtask_prompts =
                    crate::parallel_dispatch::decompose(req.prompt.as_deref().unwrap_or_default());
                if subtask_prompts.len() > 1 {
                    let project_config = harness_core::config::load_project_config(&project_root);
                    let remote = project_config.git.remote.clone();
                    let base_branch = project_config.git.base_branch.clone();

                    // Register subtask IDs in the parent before dispatch.
                    let sub_ids: Vec<TaskId> = (0..subtask_prompts.len())
                        .map(|i| TaskId(format!("{}-p{i}", id.0)))
                        .collect();
                    mutate_and_persist(&store, &id, |s| {
                        s.subtask_ids = sub_ids.clone();
                    })
                    .await;

                    let context_items = crate::task_executor::collect_context_items(
                        &skills,
                        &project_root,
                        req.prompt.as_deref().unwrap_or_default(),
                    )
                    .await;

                    let turn_timeout = tokio::time::Duration::from_secs(req.turn_timeout_secs);
                    let results = crate::parallel_dispatch::run_parallel_subtasks(
                        &id,
                        agent.clone(),
                        subtask_prompts,
                        wmgr.clone(),
                        &project_root,
                        &remote,
                        &base_branch,
                        context_items,
                        turn_timeout,
                    )
                    .await;

                    let any_success = results.iter().any(|r| r.response.is_some());
                    mutate_and_persist(&store, &id, |s| {
                        for r in &results {
                            s.rounds.push(RoundResult {
                                turn: (r.index as u32).saturating_add(1),
                                action: format!("parallel_subtask_{}", r.index),
                                result: if r.response.is_some() {
                                    "success".into()
                                } else {
                                    "failed".into()
                                },
                                detail: r.error.clone(),
                            });
                        }
                        if any_success {
                            s.status = TaskStatus::Done;
                        } else {
                            s.status = TaskStatus::Failed;
                            s.error = Some("all parallel subtasks failed".to_string());
                        }
                    })
                    .await;
                    return Ok(());
                }
            }
        }

        // If workspace isolation is configured, create a per-task git worktree.
        let run_project = if let Some(ref wmgr) = workspace_mgr {
            let project_config = harness_core::config::load_project_config(&project_root);
            wmgr.create_workspace(
                &id,
                &project_root,
                &project_config.git.remote,
                &project_config.git.base_branch,
            )
            .await?
        } else {
            project_root
        };

        let task_result = crate::task_executor::run_task(
            &store,
            &id,
            agent.as_ref(),
            reviewer.as_deref(),
            skills,
            events,
            interceptors,
            &req,
            run_project,
            server_config.as_ref(),
        )
        .await;

        // Cleanup workspace when task ends (Done or Failed) if auto_cleanup is set.
        if let Some(wmgr) = workspace_mgr {
            if wmgr.config.auto_cleanup {
                if let Err(e) = wmgr.remove_workspace(&id).await {
                    tracing::warn!("workspace cleanup failed for {id:?}: {e}");
                }
            }
        }

        task_result
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
        // Close the stream channel so SSE clients receive EOF.
        store_watcher.close_task_stream(&id_watcher);
        if let Some(cb) = completion_callback {
            match store_watcher.get(&id_watcher) {
                Some(final_state) => cb(final_state).await,
                None => tracing::warn!(
                    "completion_callback: task {:?} not found in store after completion",
                    id_watcher
                ),
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

    #[tokio::test]
    async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = TaskId("stream-test".to_string());

        // No stream registered yet.
        assert!(
            store.subscribe_task_stream(&id).is_none(),
            "subscribe before register should return None"
        );

        store.register_task_stream(&id);
        let mut rx = store
            .subscribe_task_stream(&id)
            .ok_or_else(|| anyhow::anyhow!("subscribe after register should succeed"))?;

        store.publish_stream_item(
            &id,
            StreamItem::MessageDelta {
                text: "hello\n".into(),
            },
        );
        store.publish_stream_item(&id, StreamItem::Done);

        let item1 = rx.recv().await?;
        let item2 = rx.recv().await?;
        assert!(matches!(item1, StreamItem::MessageDelta { .. }));
        assert!(matches!(item2, StreamItem::Done));

        // After close_task_stream the channel sender is dropped.
        store.close_task_stream(&id);
        assert!(
            store.subscribe_task_stream(&id).is_none(),
            "subscribe after close should return None"
        );
        Ok(())
    }

    #[tokio::test]
    async fn task_stream_backpressure_drops_oldest_on_lag() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = TaskId("backpressure-test".to_string());

        store.register_task_stream(&id);
        let mut rx = store
            .subscribe_task_stream(&id)
            .ok_or_else(|| anyhow::anyhow!("subscribe should succeed after register"))?;

        // Publish more items than TASK_STREAM_CAPACITY to trigger lag.
        for i in 0..(TASK_STREAM_CAPACITY + 10) as u64 {
            store.publish_stream_item(
                &id,
                StreamItem::MessageDelta {
                    text: format!("line {i}\n"),
                },
            );
        }

        // Receiver should see RecvError::Lagged on overflow.
        let result = rx.recv().await;
        assert!(
            result.is_err(),
            "expected Lagged error after overflow, got: {:?}",
            result
        );
        Ok(())
    }

    #[tokio::test]
    async fn list_children_returns_subtasks_for_parent() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let parent_id = TaskId("parent-task".to_string());
        let parent = TaskState::new(parent_id.clone());
        store.insert(&parent).await;

        let mut child1 = TaskState::new(TaskId("child-1".to_string()));
        child1.parent_id = Some(parent_id.clone());
        store.insert(&child1).await;

        let mut child2 = TaskState::new(TaskId("child-2".to_string()));
        child2.parent_id = Some(parent_id.clone());
        store.insert(&child2).await;

        // Unrelated task.
        store
            .insert(&TaskState::new(TaskId("other".to_string())))
            .await;

        let children = store.list_children(&parent_id);
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|c| c.parent_id.as_ref() == Some(&parent_id)));

        let no_children = store.list_children(&TaskId("nonexistent".to_string()));
        assert!(no_children.is_empty());
        Ok(())
    }

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
            req: AgentRequest,
            _tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::Result<()> {
            // Mirror execute(): capture context on first call so tests that
            // verify skill injection work whether execute or execute_stream is called.
            let mut guard = self.captured.lock().await;
            if guard.is_empty() {
                *guard = req.context.clone();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn skills_are_injected_into_agent_context() -> anyhow::Result<()> {
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut skill_store = harness_skills::SkillStore::new();
        skill_store.create(
            "test-skill".to_string(),
            "<!-- trigger-patterns: test task -->\ndo something useful".to_string(),
        );
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
            max_budget_usd: None,
            ..Default::default()
        };

        let events = Arc::new(harness_observe::EventStore::new(dir.path()).await?);
        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire().await.unwrap();
        spawn_task(
            store,
            agent_clone,
            None,
            Default::default(),
            skills,
            events,
            vec![],
            req,
            None,
            permit,
            None,
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
        let events = Arc::new(harness_observe::EventStore::new(dir.path()).await?);

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
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire().await.unwrap();
        let task_id = spawn_task(
            store.clone(),
            agent,
            None,
            Default::default(),
            skills,
            events,
            interceptors,
            req,
            None,
            permit,
            None,
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
                        detail: None,
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
        let events = Arc::new(harness_observe::EventStore::new(dir.path()).await?);

        let req = CreateTaskRequest {
            prompt: Some("panic path".into()),
            issue: None,
            pr: None,
            agent: None,
            project: None,
            wait_secs: 0,
            max_rounds: 0,
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire().await.unwrap();
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
            None,
            permit,
            None,
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
        let failure_events = events
            .query(&EventFilters {
                hook: Some("task_failure".to_string()),
                ..Default::default()
            })
            .await?;
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
