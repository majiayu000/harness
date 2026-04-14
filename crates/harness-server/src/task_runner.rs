use crate::task_db::TaskDb;
use dashmap::DashMap;
use harness_core::agent::{CodeAgent, StreamItem};
use harness_core::types::{Decision, Event, SessionId, TaskId as CoreTaskId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

pub type TaskId = CoreTaskId;

/// Broadcast channel capacity for per-task stream events.
/// When the buffer is full, the oldest events are dropped for lagging receivers.
const TASK_STREAM_CAPACITY: usize = 512;

/// Async callback invoked when a task reaches a terminal state (Done/Failed).
/// Receives a snapshot of the final `TaskState`. Implemented in the intake layer
/// to call `IntakeSource::on_task_complete` without creating circular dependencies.
pub type CompletionCallback =
    Arc<dyn Fn(TaskState) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Current phase in the task pipeline.
///
/// Tasks progress through phases sequentially. Simple tasks may skip
/// Triage/Plan and go directly to Implement.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    /// Initial phase — Tech Lead evaluates the issue.
    Triage,
    /// Architect designs the implementation plan.
    Plan,
    /// Engineer writes and tests code (default starting phase).
    #[default]
    Implement,
    /// Independent code review.
    Review,
    /// Terminal — task completed or failed.
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    AwaitingDeps,
    Implementing,
    AgentReview,
    Waiting,
    Reviewing,
    Done,
    Failed,
    Cancelled,
}

const TERMINAL_TASK_STATUSES: &[&str] = &["done", "failed", "cancelled"];
const RESUMABLE_TASK_STATUSES: &[&str] = &["implementing", "agent_review", "waiting", "reviewing"];

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub fn is_inflight(&self) -> bool {
        matches!(
            self,
            Self::Implementing | Self::AgentReview | Self::Waiting | Self::Reviewing
        )
    }

    pub fn is_resumable_after_restart(&self) -> bool {
        self.is_inflight()
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Done)
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed)
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    pub fn terminal_statuses() -> &'static [&'static str] {
        TERMINAL_TASK_STATUSES
    }

    pub fn resumable_statuses() -> &'static [&'static str] {
        RESUMABLE_TASK_STATUSES
    }
}

impl AsRef<str> for TaskStatus {
    fn as_ref(&self) -> &str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::AwaitingDeps => "awaiting_deps",
            TaskStatus::Implementing => "implementing",
            TaskStatus::AgentReview => "agent_review",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Reviewing => "reviewing",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(TaskStatus::Pending),
            "awaiting_deps" => Ok(TaskStatus::AwaitingDeps),
            "implementing" => Ok(TaskStatus::Implementing),
            "agent_review" => Ok(TaskStatus::AgentReview),
            "waiting" => Ok(TaskStatus::Waiting),
            "reviewing" => Ok(TaskStatus::Reviewing),
            "done" => Ok(TaskStatus::Done),
            "failed" => Ok(TaskStatus::Failed),
            "cancelled" => Ok(TaskStatus::Cancelled),
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
    /// Time from agent launch to first output token (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_latency_ms: Option<u64>,
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
    /// Task IDs that must reach Done before this task may start.
    /// Persisted as JSON in the database.
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    /// IDs of subtasks spawned by this task during parallel dispatch.
    /// Populated at runtime; not persisted (use `TaskStore::list_children` after restart).
    #[serde(default)]
    pub subtask_ids: Vec<TaskId>,
    /// Resolved project root for this task. Set at spawn time and persisted to the database.
    /// Used by sibling-awareness lookups and exposed via TaskSummary for observability.
    #[serde(skip)]
    pub project_root: Option<PathBuf>,
    /// GitHub issue number if this is an issue-based task. Set at spawn time; not persisted.
    #[serde(skip)]
    pub issue: Option<u64>,
    /// Repository slug (e.g. "owner/repo"). Persisted for traceability.
    pub repo: Option<String>,
    /// Short description derived from the task prompt or issue number.
    #[serde(default)]
    pub description: Option<String>,
    /// ISO 8601 creation timestamp. Set at spawn time and persisted to the tasks DB.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Scheduling priority: 0 = normal (default), 1 = high, 2 = critical.
    /// Persisted to DB and used by TaskQueue::acquire to skip lower-priority waiters.
    #[serde(default)]
    pub priority: u8,
    /// Current pipeline phase. Defaults to Implement for backward compatibility.
    #[serde(default)]
    pub phase: TaskPhase,
    /// Output from the Triage phase (Tech Lead assessment). Not persisted to DB.
    #[serde(skip)]
    pub triage_output: Option<String>,
    /// Output from the Plan phase (Architect plan). Not persisted to DB.
    #[serde(skip)]
    pub plan_output: Option<String>,
    /// Caller-specified execution limits. Persisted to the DB so that recovered
    /// tasks resume with the same budget / timeout guardrails as originally
    /// requested rather than silently falling back to server defaults.
    #[serde(skip)]
    pub request_settings: Option<PersistedRequestSettings>,
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
    /// Source-specific identifier (e.g. GitHub issue number).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Repository slug (e.g. "owner/repo").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Short description derived from the task prompt or issue number.
    #[serde(default)]
    pub description: Option<String>,
    /// ISO 8601 creation timestamp.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Current pipeline phase.
    #[serde(default)]
    pub phase: TaskPhase,
    /// Task IDs that must reach Done before this task may start.
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    /// IDs of subtasks spawned by this task during parallel dispatch.
    #[serde(default)]
    pub subtask_ids: Vec<TaskId>,
    /// Resolved project root path. Persisted to the database for observability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl TaskState {
    pub(crate) fn new(id: TaskId) -> Self {
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
            depends_on: Vec::new(),
            subtask_ids: Vec::new(),
            project_root: None,
            issue: None,
            description: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            priority: 0,
            phase: TaskPhase::default(),
            triage_output: None,
            plan_output: None,
            repo: None,
            request_settings: None,
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
            external_id: self.external_id.clone(),
            repo: self.repo.clone(),
            description: self.description.clone(),
            created_at: self.created_at.clone(),
            phase: self.phase.clone(),
            depends_on: self.depends_on.clone(),
            subtask_ids: self.subtask_ids.clone(),
            project: self
                .project_root
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// Maximum allowed scheduling priority. Values above this are rejected at the
/// API boundary to prevent scheduler-level starvation of normal-priority tasks.
/// `0` = normal (default), `1` = high, `2` = critical.
pub const MAX_TASK_PRIORITY: u8 = 2;

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
    /// Maximum review rounds. When absent, triage complexity provides the default
    /// (Low=2, Medium=system default, High=8). When explicitly set by the caller,
    /// this value always wins over triage-derived defaults.
    #[serde(default)]
    pub max_rounds: Option<u32>,
    /// Maximum total agent API calls across all phases (implementation + validation retries +
    /// review rounds). When None, falls back to the global `concurrency.max_turns` config value.
    #[serde(default)]
    pub max_turns: Option<u32>,
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
    /// Source-specific identifier (e.g. GitHub issue number). Stored in TaskState for traceability.
    #[serde(default)]
    pub external_id: Option<String>,
    /// Repository slug (e.g. "owner/repo"). Stored in TaskState for traceability.
    #[serde(default)]
    pub repo: Option<String>,
    /// Explicit parent task ID.
    #[serde(default)]
    pub parent_task_id: Option<TaskId>,
    /// Task IDs that must complete (Done) before this task starts.
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    /// Scheduling priority: 0 = normal (default), 1 = high, 2 = critical.
    /// Higher values are served first when multiple tasks are waiting for a slot.
    #[serde(default)]
    pub priority: u8,
}

/// Execution limits that survive a server restart.
///
/// Serialised as a JSON blob in `tasks.request_settings`. Restored at startup
/// redispatch so recovered tasks honour the original budget / timeout
/// guardrails instead of silently falling back to server-wide defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedRequestSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_budget_usd: Option<f64>,
    pub wait_secs: u64,
    pub retry_base_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub stall_timeout_secs: u64,
    pub turn_timeout_secs: u64,
}

impl PersistedRequestSettings {
    pub(crate) fn from_req(req: &CreateTaskRequest) -> Self {
        Self {
            agent: req.agent.clone(),
            max_rounds: req.max_rounds,
            max_turns: req.max_turns,
            max_budget_usd: req.max_budget_usd,
            wait_secs: req.wait_secs,
            retry_base_backoff_ms: req.retry_base_backoff_ms,
            retry_max_backoff_ms: req.retry_max_backoff_ms,
            stall_timeout_secs: req.stall_timeout_secs,
            turn_timeout_secs: req.turn_timeout_secs,
        }
    }

    pub(crate) fn apply_to_req(&self, req: &mut CreateTaskRequest) {
        req.agent = self.agent.clone();
        req.max_rounds = self.max_rounds;
        req.max_turns = self.max_turns;
        req.max_budget_usd = self.max_budget_usd;
        req.wait_secs = self.wait_secs;
        req.retry_base_backoff_ms = self.retry_base_backoff_ms;
        req.retry_max_backoff_ms = self.retry_max_backoff_ms;
        req.stall_timeout_secs = self.stall_timeout_secs;
        req.turn_timeout_secs = self.turn_timeout_secs;
    }
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
            max_rounds: None,
            max_turns: None,
            turn_timeout_secs: default_turn_timeout(),
            max_budget_usd: None,
            retry_base_backoff_ms: default_retry_base_backoff_ms(),
            retry_max_backoff_ms: default_retry_max_backoff_ms(),
            stall_timeout_secs: default_stall_timeout(),
            source: None,
            external_id: None,
            repo: None,
            parent_task_id: None,
            depends_on: Vec::new(),
            priority: 0,
        }
    }
}

fn summarize_request_description(req: &CreateTaskRequest) -> Option<String> {
    // Only persist structured safe labels — never raw prompt text, which may contain
    // credentials or customer data.
    if let Some(n) = req.issue {
        return Some(format!("issue #{n}"));
    }
    if let Some(n) = req.pr {
        return Some(format!("PR #{n}"));
    }
    // Prompt-only tasks: store a generic label so that:
    //   (a) sibling-awareness can include them (prevents parallel agents stomping the same files),
    //   (b) operators can identify crashed tasks in the DB/dashboard after a restart.
    // The prompt itself is deliberately not stored.
    if req.prompt.is_some() {
        return Some("prompt task".to_string());
    }
    None
}

pub(crate) async fn fill_missing_repo_from_project(req: &mut CreateTaskRequest) {
    if req.repo.is_some() {
        return;
    }
    let Some(project) = req.project.as_deref() else {
        return;
    };
    req.repo = crate::task_executor::pr_detection::detect_repo_slug(project).await;
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

/// Convert a user-facing timeout value to a Duration.
/// `0` means "no timeout" and maps to ~277 hours (effectively unlimited).
pub(crate) fn effective_turn_timeout(secs: u64) -> tokio::time::Duration {
    if secs == 0 {
        tokio::time::Duration::from_secs(999_999)
    } else {
        tokio::time::Duration::from_secs(secs)
    }
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

/// Resolve and canonicalize the project root so the caller can obtain a stable
/// semaphore key before acquiring the concurrency permit.
///
/// When `project` is `None` the main git worktree is detected automatically.
/// Symlinks, relative paths and the `None` sentinel all converge to the same
/// canonical `PathBuf`, preventing the same repository from landing in
/// different per-project buckets due to path aliasing.
///
/// This function only resolves symlinks — it does NOT enforce the HOME-boundary
/// restriction applied by `validate_project_root`. Full validation still
/// happens inside `spawn_task` once the task is running.
pub(crate) async fn resolve_canonical_project(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = resolve_project_root_with(project, detect_main_worktree).await?;
    // Best-effort canonicalize: if the path doesn't exist yet (e.g. in tests
    // using a path that will be created later) fall back to the raw path so
    // we at least get a consistent string key.
    Ok(raw.canonicalize().unwrap_or(raw))
}

async fn log_task_failure_event(
    events: &harness_observe::event_store::EventStore,
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

/// Patterns that indicate a transient (retryable) failure rather than a permanent one.
const TRANSIENT_PATTERNS: &[&str] = &[
    "at capacity",
    "rate limit",
    "rate_limit",
    "hit your limit",
    "429",
    "502 Bad Gateway",
    "503 Service",
    "overloaded",
    "connection reset",
    "connection refused",
    "broken pipe",
    "EOF",
    "stream idle timeout",
    "stream stall",
    "ECONNRESET",
    "ETIMEDOUT",
    // SQLite transient contention — SQLITE_BUSY / SQLITE_LOCKED
    "database is locked",
    "database table is locked",
    "SQLITE_BUSY",
    "SQLITE_LOCKED",
];

/// Pattern indicating the CLI account-level usage limit has been reached.
/// When detected, the global rate-limit circuit breaker is activated to
/// prevent all other tasks from wasting turns.
const ACCOUNT_LIMIT_PATTERN: &str = "hit your limit";

/// Maximum number of automatic retries for transient failures.
const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Return `true` when a free-text prompt is complex enough to require a Plan phase
/// before implementation.
///
/// Heuristic: prompt longer than 200 words OR contains 3 or more file-path-like
/// tokens (sequences containing `/` or ending in a recognised source extension).
pub(crate) fn prompt_requires_plan(prompt: &str) -> bool {
    let word_count = prompt.split_whitespace().count();
    if word_count > 200 {
        return true;
    }
    let file_path_count = prompt
        .split_whitespace()
        .filter(|tok| {
            // Exclude XML/HTML tags — `</foo>` contains '/' but is not a file path.
            // System prompts wrap user data in `<external_data>...</external_data>`
            // which would otherwise trigger false positives.
            let is_xml_tag = tok.starts_with('<');
            if is_xml_tag {
                return false;
            }
            tok.contains('/') || {
                let lower = tok.to_lowercase();
                lower.ends_with(".rs")
                    || lower.ends_with(".ts")
                    || lower.ends_with(".tsx")
                    || lower.ends_with(".go")
                    || lower.ends_with(".py")
                    || lower.ends_with(".toml")
                    || lower.ends_with(".json")
            }
        })
        .count();
    file_path_count >= 3
}

fn is_non_decomposable_prompt_source(source: Option<&str>) -> bool {
    matches!(source, Some("periodic_review") | Some("sprint_planner"))
}

/// Check if an error message indicates a transient failure that may succeed on retry.
fn is_transient_error(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    TRANSIENT_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
}

/// Record a task failure, marking the task as failed.
async fn record_task_failure(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: reason.clone(),
    });
    if let Err(e) = mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.error = Some(reason);
    })
    .await
    {
        tracing::error!("failed to persist task failure for {task_id:?}: {e}");
    }
}

/// In-memory cache + SQLite persistence.
/// Per-project done/failed task counts derived from the in-memory cache.
#[derive(Debug, Default, Clone)]
pub struct ProjectCounts {
    pub done: u64,
    pub failed: u64,
}

/// Combined global and per-project done/failed counts produced by a single
/// cache scan, avoiding both full task cloning and double iteration.
#[derive(Debug)]
pub struct DashboardCounts {
    pub global_done: u64,
    pub global_failed: u64,
    pub by_project: HashMap<String, ProjectCounts>,
}

/// Lightweight inputs collected for LLM metrics computation.
///
/// Bounded inputs for LLM metrics computation, collected in two O(1) phases:
/// cache iteration (active tasks) then bounded SQL queries (terminal tasks).
#[derive(Debug)]
pub struct LlmMetricsInputs {
    /// Non-zero turn counts from both active (cache) and terminal (DB) tasks.
    pub turn_counts: Vec<u32>,
    /// First real first-token latency per task (milliseconds), from cache and DB.
    pub first_token_latencies: Vec<u64>,
}

pub struct TaskStore {
    pub(crate) cache: DashMap<TaskId, TaskState>,
    db: TaskDb,
    persist_locks: DashMap<TaskId, Arc<Mutex<()>>>,
    /// Per-task broadcast channels for real-time stream forwarding to SSE clients.
    stream_txs: DashMap<TaskId, broadcast::Sender<StreamItem>>,
    /// Per-task abort handles for cooperative cancellation via Tokio task abort.
    abort_handles: DashMap<TaskId, tokio::task::AbortHandle>,
    /// Global circuit breaker: when the CLI account-level limit is hit,
    /// all tasks pause until this instant passes.
    rate_limit_until: RwLock<Option<tokio::time::Instant>>,
    /// Append-only JSONL event log for crash recovery. `None` if the file
    /// could not be opened (best-effort; server still starts without it).
    pub(crate) event_log: Option<Arc<crate::event_replay::TaskEventLog>>,
}

impl TaskStore {
    pub async fn open(db_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let db = TaskDb::open(db_path).await?;

        // 1. Event replay: runs BEFORE recover_in_progress so event-sourced
        //    data (pr_url, terminal status) wins over checkpoint data.
        let event_log_path = db_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("task-events.jsonl");
        if let Err(e) = crate::event_replay::replay_and_recover(&db, &event_log_path).await {
            tracing::warn!("startup: event replay failed (non-fatal): {e}");
        }

        // 2. Legacy checkpoint-based recovery as fallback.
        let recovery = db.recover_in_progress().await?;
        if recovery.resumed > 0 {
            tracing::info!(
                "startup recovery: resumed {} task(s) from checkpoint",
                recovery.resumed
            );
        }
        if recovery.failed > 0 {
            tracing::warn!(
                "startup recovery: marked {} interrupted task(s) as failed (no fresh checkpoint)",
                recovery.failed
            );
        }

        // 3. Open the event log for appending during this server session.
        let event_log = match crate::event_replay::TaskEventLog::open(&event_log_path) {
            Ok(log) => {
                tracing::debug!("task event log: {}", event_log_path.display());
                Some(Arc::new(log))
            }
            Err(e) => {
                tracing::warn!(
                    "failed to open task event log at {}: {e}",
                    event_log_path.display()
                );
                None
            }
        };

        let cache = DashMap::new();
        let persist_locks = DashMap::new();
        // Only load active (non-terminal) tasks into the in-memory cache to prevent
        // unbounded memory growth from historical completed tasks.
        let active_statuses = &[
            "pending",
            "awaiting_deps",
            "implementing",
            "agent_review",
            "waiting",
            "reviewing",
        ];
        for task in db.list_by_status(active_statuses).await? {
            persist_locks.insert(task.id.clone(), Arc::new(Mutex::new(())));
            cache.insert(task.id.clone(), task);
        }
        let store = Arc::new(Self {
            cache,
            db,
            persist_locks,
            stream_txs: DashMap::new(),
            abort_handles: DashMap::new(),
            rate_limit_until: RwLock::new(None),
            event_log,
        });
        Ok(store)
    }

    pub fn get(&self, id: &TaskId) -> Option<TaskState> {
        self.cache.get(id).map(|r| r.value().clone())
    }

    /// Look up a task by ID, checking the in-memory cache first.
    /// Falls back to the database for terminal tasks that were evicted from
    /// the cache at startup (Done, Failed, Cancelled).
    /// Returns `Ok(None)` only when the ID is unknown in both cache and DB.
    /// Returns `Err` when the database query itself fails.
    pub async fn get_with_db_fallback(&self, id: &TaskId) -> anyhow::Result<Option<TaskState>> {
        if let Some(task) = self.cache.get(id) {
            return Ok(Some(task.value().clone()));
        }
        self.db.get(id.0.as_str()).await
    }

    /// Return the status of a dependency task with a single DB lookup.
    ///
    /// Checks the in-memory cache first; falls back to a lightweight
    /// `SELECT status` query (no `rounds` JSON decode) for terminal tasks
    /// evicted from the startup cache.  Returns `None` when the task is
    /// unknown in both cache and DB, or when the DB call fails.
    async fn dep_status(&self, id: &TaskId) -> Option<TaskStatus> {
        if let Some(task) = self.cache.get(id) {
            return Some(task.status.clone());
        }
        match self.db.get_status_only(id.0.as_str()).await {
            Ok(Some(s)) => s.parse::<TaskStatus>().ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(task_id = %id.0, "DB error fetching dep status: {e}; treating as absent");
                None
            }
        }
    }

    /// Return IDs of terminal tasks (Done, Failed, Cancelled) directly from the database.
    ///
    /// Used during startup for worktree cleanup. Only fetches task IDs to avoid
    /// deserializing the heavy `rounds` column for large historical datasets.
    pub async fn list_terminal_ids_from_db(&self) -> anyhow::Result<Vec<TaskId>> {
        let ids = self
            .db
            .list_ids_by_status(&["done", "failed", "cancelled"])
            .await?;
        Ok(ids.into_iter().map(harness_core::types::TaskId).collect())
    }

    pub fn count(&self) -> usize {
        self.cache.len()
    }

    /// Check whether an active (non-terminal) task already exists for the same
    /// project + external_id. Cache-first, DB fallback.
    pub async fn find_active_duplicate(
        &self,
        project_id: &str,
        external_id: &str,
    ) -> Option<TaskId> {
        let mut found_terminal_in_cache = false;
        for entry in self.cache.iter() {
            let task = entry.value();
            let same_key = task.external_id.as_deref() == Some(external_id)
                && task
                    .project_root
                    .as_ref()
                    .map(|p| p.to_string_lossy() == project_id)
                    .unwrap_or(false);
            if !same_key {
                continue;
            }
            if !matches!(
                task.status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                return Some(task.id.clone());
            }
            // Cache has a terminal match — skip DB fallback since cache is
            // more authoritative and the DB row may be stale.
            found_terminal_in_cache = true;
        }
        if found_terminal_in_cache {
            return None;
        }
        match self.db.find_active_duplicate(project_id, external_id).await {
            Ok(Some(id)) => Some(harness_core::types::TaskId(id)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("dedup: DB lookup failed: {e}");
                None
            }
        }
    }

    /// Check whether a `done` task with a PR URL already exists for the same
    /// project + external_id. Cache-first, DB fallback. Fail-open on DB errors.
    pub async fn find_terminal_pr_duplicate(
        &self,
        project_id: &str,
        external_id: &str,
    ) -> Option<(TaskId, String)> {
        for entry in self.cache.iter() {
            let task = entry.value();
            let same_key = task.external_id.as_deref() == Some(external_id)
                && task
                    .project_root
                    .as_ref()
                    .map(|p| p.to_string_lossy() == project_id)
                    .unwrap_or(false);
            if same_key && matches!(task.status, TaskStatus::Done) {
                if let Some(ref url) = task.pr_url {
                    return Some((task.id.clone(), url.clone()));
                }
            }
        }
        match self.db.find_terminal_with_pr(project_id, external_id).await {
            Ok(Some((id, url))) => Some((harness_core::types::TaskId(id), url)),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("dedup: terminal PR DB lookup failed: {e}");
                None
            }
        }
    }

    /// Return all tasks currently in the in-memory cache.
    ///
    /// **Semantic note**: since startup only loads active (non-terminal) tasks
    /// into the cache, this method returns only Pending, AwaitingDeps,
    /// Implementing, AgentReview, Waiting, and Reviewing tasks.
    /// To look up a specific completed task use [`get_with_db_fallback`].
    /// To enumerate all tasks including historical ones use [`list_all_with_terminal`].
    pub fn list_all(&self) -> Vec<TaskState> {
        self.cache.iter().map(|e| e.value().clone()).collect()
    }

    /// Return all tasks: active ones from cache (most current state) merged
    /// with terminal tasks from the database (Done, Failed, Cancelled).
    ///
    /// Cache entries take precedence over DB rows for the same task ID so that
    /// in-flight state is always reflected accurately.
    pub async fn list_all_with_terminal(&self) -> anyhow::Result<Vec<TaskState>> {
        // DB is the authoritative record of all tasks ever created.
        let mut by_id: std::collections::HashMap<TaskId, TaskState> = self
            .db
            .list()
            .await?
            .into_iter()
            .map(|t| (t.id.clone(), t))
            .collect();
        // Override with cache values — these carry the live in-flight state.
        for entry in self.cache.iter() {
            by_id.insert(entry.key().clone(), entry.value().clone());
        }
        Ok(by_id.into_values().collect())
    }

    /// Return all tasks as lightweight [`TaskSummary`] values.
    ///
    /// Fetches summary columns from the DB (skipping the heavy `rounds` field),
    /// then overrides with live cache entries so in-flight state is accurate.
    /// Use this in the `/tasks` list endpoint instead of `list_all_with_terminal`.
    pub async fn list_all_summaries_with_terminal(&self) -> anyhow::Result<Vec<TaskSummary>> {
        let mut by_id: std::collections::HashMap<TaskId, TaskSummary> = self
            .db
            .list_summaries()
            .await?
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect();
        for entry in self.cache.iter() {
            by_id.insert(entry.key().clone(), entry.value().summary());
        }
        Ok(by_id.into_values().collect())
    }

    /// Return `(TaskId, TaskStatus)` pairs for all tasks without deserializing `rounds`.
    ///
    /// Hot-path callers (skill governance, token usage attribution) that only need
    /// task status should use this instead of `list_all_with_terminal`.
    pub async fn list_all_statuses_with_terminal(
        &self,
    ) -> anyhow::Result<HashMap<TaskId, TaskStatus>> {
        let mut by_id: HashMap<TaskId, TaskStatus> = self
            .db
            .list_id_status()
            .await?
            .into_iter()
            .filter_map(|(id, status)| {
                status
                    .parse::<TaskStatus>()
                    .ok()
                    .map(|s| (harness_core::types::TaskId(id), s))
            })
            .collect();
        for entry in self.cache.iter() {
            by_id.insert(entry.key().clone(), entry.value().status.clone());
        }
        Ok(by_id)
    }

    /// Run `f` only if the task still exists and is live-`pending`.
    ///
    /// Holding the mutable cache guard across `f` prevents a concurrent status
    /// transition from changing this task out of `pending` between the check
    /// and a follow-up side effect such as runtime-host lease insertion.
    pub(crate) fn with_task_if_pending<R>(&self, id: &TaskId, f: impl FnOnce() -> R) -> Option<R> {
        let entry = self.cache.get_mut(id)?;
        if !matches!(entry.status, TaskStatus::Pending) {
            return None;
        }
        Some(f())
    }

    /// Return the `pr_url` of the most recently created Done task, ordered by `created_at DESC`
    /// from the database (stable ordering, unlike the in-memory DashMap cache).
    pub async fn latest_done_pr_url(&self) -> Option<String> {
        match self.db.latest_done_pr_url().await {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!("failed to query latest done PR URL: {e}");
                None
            }
        }
    }

    /// Return the `pr_url` of the most recent Done task for a specific project root path.
    pub async fn latest_done_pr_url_by_project(&self, project: &str) -> Option<String> {
        match self.db.latest_done_pr_url_by_project(project).await {
            Ok(url) => url,
            Err(e) => {
                tracing::warn!("failed to query latest done PR URL for project {project}: {e}");
                None
            }
        }
    }

    /// Fetch the latest done PR URL for every project in a single DB query.
    /// Returns a map from project root path string to PR URL.
    pub async fn latest_done_pr_urls_all_projects(&self) -> HashMap<String, String> {
        match self.db.latest_done_pr_urls_all_projects().await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!("failed to bulk-query latest done PR URLs: {e}");
                HashMap::new()
            }
        }
    }

    /// Return all inflight sibling tasks on the same project, excluding `exclude_id`.
    ///
    /// Used by `run_task` to build sibling-awareness context before starting implementation,
    /// so each agent knows what other agents are working on and avoids touching their files.
    /// Only tasks that have `project_root` set (populated at spawn time) are considered.
    pub fn list_siblings(&self, project: &std::path::Path, exclude_id: &TaskId) -> Vec<TaskState> {
        self.cache
            .iter()
            .filter(|e| {
                let task = e.value();
                &task.id != exclude_id
                    && (matches!(task.status, TaskStatus::Pending) || task.status.is_inflight())
                    && task.project_root.as_deref() == Some(project)
            })
            .map(|e| e.value().clone())
            .collect()
    }

    /// Compute global and per-project done/failed counts via SQL aggregation.
    ///
    /// Delegates to `TaskDb` so the count scales with the database engine rather
    /// than requiring an O(N) scan of the in-memory cache, which grows unboundedly
    /// as tasks accumulate. Uses the `idx_tasks_project_status_updated` index.
    pub async fn count_for_dashboard(&self) -> DashboardCounts {
        match self.db.count_done_failed_by_project().await {
            Ok((global_done, global_failed, rows)) => {
                let by_project = rows
                    .into_iter()
                    .map(|(project, done, failed)| (project, ProjectCounts { done, failed }))
                    .collect();
                DashboardCounts {
                    global_done,
                    global_failed,
                    by_project,
                }
            }
            Err(e) => {
                tracing::warn!("count_for_dashboard: SQL aggregation failed: {e}");
                DashboardCounts {
                    global_done: 0,
                    global_failed: 0,
                    by_project: HashMap::new(),
                }
            }
        }
    }

    /// Collect lightweight inputs for LLM metrics computation.
    ///
    /// Both **turn counts** and **first-token latencies** are collected in two
    /// bounded phases so the dashboard poll remains O(1) regardless of total
    /// task history size:
    ///
    /// 1. **Cache phase** — iterate `DashMap` refs in-place (no full `TaskState`
    ///    clone).  Records the IDs seen so phase 2 can deduplicate.
    /// 2. **DB phase** — bounded SQL queries (`LIMIT 500`, `ORDER BY updated_at DESC`)
    ///    return only scalar columns (`turn`, `first_token_latency_ms`) for the
    ///    500 most-recently-completed terminal tasks not already counted from cache.
    ///    This covers the idle-queue restart case without an unbounded table scan.
    ///
    /// Rounds whose `result` is `"resumed_checkpoint"` are skipped in both
    /// sources: they are synthetic markers with no real latency data.
    pub async fn collect_llm_metrics_inputs(&self) -> LlmMetricsInputs {
        // Phase 1: iterate cache refs in-place; never clone full TaskState.
        // Collect both turn counts and latencies in a single pass, and track
        // IDs seen so we can deduplicate against the DB queries in phase 2.
        //
        // Only terminal (done/failed) tasks are counted so the cache phase matches
        // DB phase semantics: both sources use the same status set.  Non-terminal
        // task IDs are still inserted into `cache_ids` so the DB deduplication
        // logic remains correct (we never double-count a task that transitions to
        // terminal while the DB query is in-flight).
        let mut cache_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut turn_counts: Vec<u32> = Vec::new();
        let mut first_token_latencies: Vec<u64> = Vec::new();
        for entry in self.cache.iter() {
            let task = entry.value();
            cache_ids.insert(entry.key().0.clone());

            // Skip non-terminal tasks to match the DB phase (done/failed only).
            if !matches!(task.status, TaskStatus::Done | TaskStatus::Failed) {
                continue;
            }

            if task.turn > 0 {
                turn_counts.push(task.turn);
            }

            // Skip the synthetic resumed_checkpoint round injected at recovery time,
            // then find the first round that actually received a response token.
            // Using find_map (instead of find + and_then) means transient_retry rounds
            // whose first_token_latency_ms is None are also skipped, so a task with
            // one or more transient failures before a successful round is still included
            // in p50 using the latency of that successful round.
            if let Some(latency) = task
                .rounds
                .iter()
                .filter(|r| r.result != "resumed_checkpoint")
                .find_map(|r| r.first_token_latency_ms)
            {
                first_token_latencies.push(latency);
            }
        }

        // Phase 2a: bounded DB query for terminal turn counts not in cache.
        // After restart with idle queue the cache is empty; without this, turn
        // counts would be zero even though history is persisted in the DB.
        // `list_terminal_turn_counts` fetches only `id` and `turn` (no rounds
        // blob) and is bounded to 500 rows ordered by updated_at DESC.
        match self.db.list_terminal_turn_counts().await {
            Ok(rows) => {
                for (id, turn) in rows {
                    if !cache_ids.contains(&id) && turn > 0 {
                        turn_counts.push(turn as u32);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("collect_llm_metrics_inputs: DB turn counts query failed: {e}");
            }
        }

        // Phase 2b: bounded DB query for terminal latencies not in cache.
        // `list_terminal_first_token_latencies_ms` extracts the scalar in SQL via
        // json_extract (no full rounds blob in Rust memory) and is bounded to the
        // 500 most-recently-completed terminal tasks so each dashboard poll stays O(1).
        match self.db.list_terminal_first_token_latencies_ms().await {
            Ok(rows) => {
                for (id, latency_opt) in rows {
                    if cache_ids.contains(&id) {
                        // Already counted via cache iteration above.
                        continue;
                    }
                    if let Some(ms) = latency_opt {
                        first_token_latencies.push(ms as u64);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("collect_llm_metrics_inputs: DB terminal latency query failed: {e}");
            }
        }

        LlmMetricsInputs {
            turn_counts,
            first_token_latencies,
        }
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

    /// Store the abort handle for a running task so it can be cancelled later.
    pub(crate) fn store_abort_handle(&self, id: &TaskId, handle: tokio::task::AbortHandle) {
        self.abort_handles.insert(id.clone(), handle);
    }

    /// Remove and discard the abort handle when the task finishes normally.
    pub(crate) fn remove_abort_handle(&self, id: &TaskId) {
        self.abort_handles.remove(id);
    }

    /// Abort the running task's Tokio future, if one is registered.
    /// The `kill_on_drop(true)` flag on child processes ensures the CLI subprocess
    /// is also killed when the future is dropped after abort.
    /// Returns `true` if an abort handle was found and triggered.
    pub fn abort_task(&self, id: &TaskId) -> bool {
        if let Some(handle) = self.abort_handles.get(id) {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Activate the global rate-limit circuit breaker. All tasks will pause
    /// before their next agent call until `duration` elapses.
    pub async fn set_rate_limit(&self, duration: std::time::Duration) {
        let until = tokio::time::Instant::now() + duration;
        *self.rate_limit_until.write().await = Some(until);
        tracing::warn!(
            pause_secs = duration.as_secs(),
            "rate-limit circuit breaker activated — all tasks paused"
        );
    }

    /// Wait if the global rate-limit circuit breaker is active.
    /// Returns only after the deadline is None or already past. Loops to handle
    /// the case where a concurrent `set_rate_limit()` extends the deadline while
    /// this task is sleeping — without looping, the task would proceed after the
    /// original deadline and burn turns / trigger extra 429s.
    pub async fn wait_for_rate_limit(&self) {
        loop {
            let deadline = { *self.rate_limit_until.read().await };
            let Some(until) = deadline else { return };
            let now = tokio::time::Instant::now();
            if now >= until {
                // Deadline already passed. Clear it if unchanged; if a concurrent
                // setter extended the deadline, re-loop to wait for the new value.
                let mut wl = self.rate_limit_until.write().await;
                if *wl == Some(until) {
                    *wl = None;
                    return;
                }
                // Deadline was extended concurrently — fall through to re-loop.
                continue;
            }
            let remaining = until - now;
            tracing::info!(
                remaining_secs = remaining.as_secs(),
                "task waiting for rate-limit circuit breaker to clear"
            );
            tokio::time::sleep_until(until).await;
            // Only clear the breaker if it still holds the same deadline we
            // snapshotted. A concurrent set_rate_limit() call may have extended
            // the deadline during our sleep; preserving the new deadline lets
            // the next loop iteration wait for the extension.
            {
                let mut wl = self.rate_limit_until.write().await;
                if *wl == Some(until) {
                    *wl = None;
                }
            }
            // Loop again: re-read the deadline in case it was extended.
        }
    }

    /// Check if the rate-limit circuit breaker is currently active.
    pub async fn is_rate_limited(&self) -> bool {
        let deadline = *self.rate_limit_until.read().await;
        match deadline {
            Some(until) => tokio::time::Instant::now() < until,
            None => false,
        }
    }

    /// Persist an artifact captured from agent output during task execution.
    pub(crate) async fn insert_artifact(
        &self,
        task_id: &TaskId,
        turn: u32,
        artifact_type: &str,
        content: &str,
    ) {
        if let Err(e) = self
            .db
            .insert_artifact(&task_id.0, turn, artifact_type, content)
            .await
        {
            tracing::warn!(task_id = %task_id.0, artifact_type, "failed to insert task artifact: {e}");
        }
    }

    /// Return all artifacts for a task ordered by insertion time.
    pub async fn list_artifacts(
        &self,
        task_id: &TaskId,
    ) -> anyhow::Result<Vec<crate::task_db::TaskArtifact>> {
        self.db.list_artifacts(&task_id.0).await
    }

    /// Append a [`TaskEvent`] to the event log. No-op when the log is not open.
    pub(crate) fn log_event(&self, event: crate::event_replay::TaskEvent) {
        if let Some(ref log) = self.event_log {
            log.append(&event);
        }
    }

    pub(crate) async fn insert(&self, state: &TaskState) {
        self.persist_locks
            .entry(state.id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())));
        self.cache.insert(state.id.clone(), state.clone());
        if let Err(e) = self.db.insert(state).await {
            tracing::error!("task_db insert failed: {e}");
        }
        self.log_event(crate::event_replay::TaskEvent::Created {
            task_id: state.id.0.clone(),
            ts: crate::event_replay::now_ts(),
        });
    }

    /// Write a phase checkpoint for `task_id`. Checkpoint writes are non-fatal:
    /// callers should log the error rather than failing the task.
    pub(crate) async fn write_checkpoint(
        &self,
        task_id: &TaskId,
        triage_output: Option<&str>,
        plan_output: Option<&str>,
        pr_url: Option<&str>,
        last_phase: &str,
    ) -> anyhow::Result<()> {
        self.db
            .write_checkpoint(
                task_id.as_str(),
                triage_output,
                plan_output,
                pr_url,
                last_phase,
            )
            .await
    }

    /// Load the checkpoint for `task_id`, or `None` if no checkpoint exists.
    pub(crate) async fn load_checkpoint(
        &self,
        task_id: &TaskId,
    ) -> anyhow::Result<Option<crate::task_db::TaskCheckpoint>> {
        self.db.load_checkpoint(task_id.as_str()).await
    }

    /// Return pending tasks that have a plan or triage checkpoint but no `pr_url`.
    ///
    /// Used at startup to re-dispatch tasks recovered from plan/triage checkpoints
    /// that were not caught by the PR-based redispatch path.
    pub(crate) async fn pending_tasks_with_checkpoint(
        &self,
    ) -> anyhow::Result<Vec<(TaskState, crate::task_db::TaskCheckpoint)>> {
        self.db.pending_tasks_with_checkpoint().await
    }

    pub(crate) async fn persist(&self, id: &TaskId) -> anyhow::Result<()> {
        let lock = self
            .persist_locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let snapshot = self.cache.get(id).map(|state| state.value().clone());
        if let Some(state) = snapshot {
            self.db.update(&state).await?;

            // Persist a checkpoint to the DB so that recover_in_progress() can
            // restore the task if the server is killed mid-flight. Only written
            // for statuses that can be resumed after restart.
            if state.status.is_resumable_after_restart() {
                let phase_str = format!("{:?}", state.phase);
                if let Err(e) = self
                    .db
                    .write_checkpoint(id.as_str(), None, None, state.pr_url.as_deref(), &phase_str)
                    .await
                {
                    tracing::warn!(
                        task_id = %id.0,
                        "checkpoint: failed to write DB checkpoint: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Validate recovered pending tasks by checking their GitHub PR state via `gh`.
    ///
    /// Spawned as a background task from `http.rs` after the completion callback is
    /// built, so it does not block server startup. For each pending task that has a
    /// `pr_url`, fetches the current PR state with a per-call timeout:
    /// - MERGED → mark Done  (completion_callback invoked)
    /// - CLOSED → mark Failed (completion_callback invoked so intake sources can
    ///   remove the issue from their `dispatched` map and allow retry)
    /// - OPEN   → leave as Pending
    ///
    /// `gh` CLI failures are treated as transient network errors; the task is left
    /// Pending so it will be retried normally.
    pub async fn validate_recovered_tasks(&self, completion_callback: Option<CompletionCallback>) {
        let candidates: Vec<(TaskId, String)> = self
            .cache
            .iter()
            .filter_map(|e| {
                let task = e.value();
                if matches!(task.status, TaskStatus::Pending) {
                    task.pr_url
                        .as_ref()
                        .map(|url| (task.id.clone(), url.clone()))
                } else {
                    None
                }
            })
            .collect();

        if candidates.is_empty() {
            return;
        }

        tracing::info!(
            "startup: validating {} recovered pending task(s) with PR URLs",
            candidates.len()
        );

        for (task_id, pr_url) in candidates {
            let Some((owner, repo, number)) = parse_pr_url(&pr_url) else {
                tracing::warn!(
                    task_id = %task_id.0,
                    pr_url,
                    "could not parse PR URL; leaving pending"
                );
                continue;
            };

            let pr_ref = format!("{owner}/{repo}#{number}");
            // kill_on_drop(true) ensures the child process is killed when the
            // timeout future is dropped, preventing zombie `gh` processes during
            // startup when many tasks are recovered concurrently.
            let mut cmd = tokio::process::Command::new("gh");
            cmd.args(["pr", "view", &pr_ref, "--json", "state", "--jq", ".state"])
                .kill_on_drop(true);
            let gh_result =
                tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output()).await;

            let output = match gh_result {
                Err(_elapsed) => {
                    tracing::warn!(
                        task_id = %task_id.0,
                        pr_url,
                        "gh pr view timed out after 10s; leaving pending"
                    );
                    continue;
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        task_id = %task_id.0,
                        pr_url,
                        "gh CLI error: {e}; leaving pending"
                    );
                    continue;
                }
                Ok(Ok(out)) => out,
            };

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    task_id = %task_id.0,
                    pr_url,
                    "gh pr view failed: {stderr}; leaving pending"
                );
                continue;
            }

            let state = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_uppercase();
            let new_status = match state.as_str() {
                "MERGED" => Some(TaskStatus::Done),
                "CLOSED" => Some(TaskStatus::Failed),
                _ => None,
            };

            if let Some(status) = new_status {
                if let Some(mut entry) = self.cache.get_mut(&task_id) {
                    entry.status = status;
                }
                // Persist before invoking the callback. If persist fails the task
                // remains `pending` in SQLite; firing the callback anyway would push
                // external state (Feishu notifications, GitHub intake cleanup) into a
                // terminal state while the DB still thinks the task is pending. On
                // the next restart the same task would be recovered and trigger the
                // same side-effects again (state split). Skip the callback so the
                // task can be safely retried on the next restart.
                if let Err(e) = self.persist(&task_id).await {
                    tracing::error!(
                        task_id = %task_id.0,
                        "failed to persist PR state update: {e}; skipping completion callback to avoid state split"
                    );
                    continue;
                }
                tracing::info!(
                    task_id = %task_id.0,
                    pr_url,
                    "startup recovery: PR state {state} → task status updated"
                );
                if let Some(cb) = &completion_callback {
                    if let Some(final_state) = self.get(&task_id) {
                        cb(final_state).await;
                    }
                }
            } else {
                tracing::info!(
                    task_id = %task_id.0,
                    pr_url,
                    "startup recovery: PR state {state} → leaving pending"
                );
            }
        }
    }
}

/// Extract `(owner, repo, number)` from a GitHub PR URL.
///
/// Expects format: `https://github.com/{owner}/{repo}/pull/{number}[#...]`
fn parse_pr_url(pr_url: &str) -> Option<(String, String, u64)> {
    // Strip fragment (e.g. #discussion_r...)
    let url = pr_url.split('#').next().unwrap_or(pr_url);
    let parts: Vec<&str> = url.trim_end_matches('/').split('/').collect();
    // Expected: ["https:", "", "github.com", owner, repo, "pull", number]
    let pull_idx = parts.iter().rposition(|&p| p == "pull")?;
    if pull_idx + 1 >= parts.len() || pull_idx < 2 {
        return None;
    }
    let number: u64 = parts[pull_idx + 1].parse().ok()?;
    let repo = parts[pull_idx - 1].to_string();
    let owner = parts[pull_idx - 2].to_string();
    Some((owner, repo, number))
}

pub async fn spawn_task(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
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
        None,
        None,
    )
    .await
}

/// Register a task with Pending status and return its ID immediately, without waiting
/// for a concurrency permit. Pair with `spawn_preregistered_task` (called from a
/// background tokio task after `task_queue.acquire()`) to begin execution.
pub async fn register_pending_task(store: Arc<TaskStore>, req: &CreateTaskRequest) -> TaskId {
    let task_id = TaskId::new();
    let mut state = TaskState::new(task_id.clone());
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.parent_id = req.parent_task_id.clone();
    state.depends_on = req.depends_on.clone();
    state.priority = req.priority;
    state.issue = req.issue;
    state.description = summarize_request_description(req);
    state.request_settings = Some(PersistedRequestSettings::from_req(req));
    store.insert(&state).await;
    // Register stream channel now so SSE clients can subscribe before execution begins.
    store.register_task_stream(&task_id);
    task_id
}

/// Begin execution for a task pre-registered via `register_pending_task`.
/// The caller must have already acquired a concurrency permit; it is held for the task's lifetime.
/// `group_permit` is an optional semaphore permit that serialises tasks within a conflict group;
/// it is held inside the innermost spawned future for the full duration of task execution.
pub async fn spawn_preregistered_task(
    task_id: TaskId,
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    req: CreateTaskRequest,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) {
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
        Some(task_id),
        group_permit,
    )
    .await;
}

async fn spawn_task_with_worktree_detector<F>(
    store: Arc<TaskStore>,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
    server_config: std::sync::Arc<harness_core::config::HarnessConfig>,
    skills: Arc<RwLock<harness_skills::store::SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    mut req: CreateTaskRequest,
    detect_worktree: F,
    workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    permit: crate::task_queue::TaskPermit,
    completion_callback: Option<CompletionCallback>,
    preregistered_id: Option<TaskId>,
    group_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> TaskId
where
    F: Fn() -> PathBuf + Send + Sync + 'static,
{
    let task_id = if let Some(id) = preregistered_id {
        // Task was pre-registered (e.g. by register_pending_task for batch submission);
        // store.insert and register_task_stream were already called.
        id
    } else {
        let task_id = TaskId::new();
        let mut state = TaskState::new(task_id.clone());
        state.source = req.source.clone();
        state.external_id = req.external_id.clone();
        state.repo = req.repo.clone();
        state.priority = req.priority;
        store.insert(&state).await;
        // Register stream channel before spawning so SSE clients can subscribe immediately.
        store.register_task_stream(&task_id);
        task_id
    };

    let id = task_id.clone();
    let store_watcher = store.clone();
    let events_watcher = events.clone();
    let id_watcher = id.clone();
    let interceptors = Arc::new(interceptors);
    let detect_worktree = Arc::new(detect_worktree);
    // Clones used to store the abort handle after the main future is spawned
    // (store and id are moved into the spawn closure).
    let store_for_abort = store.clone();
    let id_for_abort = id.clone();

    let handle = tokio::spawn(async move {
        // Hold both permits for the task's lifetime so that the group serialisation
        // semaphore is not released until actual execution completes (not just until
        // spawn_preregistered_task returns, which happens almost immediately).
        let _permit = permit;
        let _group_permit = group_permit;
        let detect_worktree = detect_worktree.clone();
        let raw_project =
            resolve_project_root_with(req.project.clone(), move || detect_worktree()).await?;
        let home_dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| raw_project.clone());
        let project_root = crate::handlers::validate_project_root(&raw_project, &home_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if req.repo.is_none() {
            req.repo = crate::task_executor::pr_detection::detect_repo_slug(&project_root).await;
        }
        let description = summarize_request_description(&req);
        if let Some(mut entry) = store.cache.get_mut(&id) {
            entry.source = req.source.clone();
            entry.external_id = req.external_id.clone();
            entry.repo = req.repo.clone();
            entry.parent_id = req.parent_task_id.clone();
            entry.depends_on = req.depends_on.clone();
            entry.project_root = Some(project_root.clone());
            entry.issue = req.issue;
            entry.description = description;
        }

        // Parallel dispatch for Complex+ prompt-only tasks when workspace isolation is active.
        // Issue and PR tasks have their own structured prompt flow and are not decomposed.
        let classification = crate::complexity_router::classify(
            req.prompt.as_deref().unwrap_or_default(),
            req.issue,
            req.pr,
        );
        let is_complex = matches!(
            classification.complexity,
            harness_core::agent::TaskComplexity::Complex
                | harness_core::agent::TaskComplexity::Critical
        );
        let is_non_decomposable_source = is_non_decomposable_prompt_source(req.source.as_deref());
        if req.issue.is_none() && req.pr.is_none() && is_complex && !is_non_decomposable_source {
            if let Some(ref wmgr) = workspace_mgr {
                let mut subtask_specs = match crate::parallel_dispatch::decompose(
                    req.prompt.as_deref().unwrap_or_default(),
                ) {
                    Ok(specs) => specs,
                    Err(msg) => {
                        tracing::warn!(task_id = %id.0, "parallel_dispatch rejected: {}", msg);
                        mutate_and_persist(&store, &id, |s| {
                            s.status = TaskStatus::Failed;
                            s.error = Some(msg);
                        })
                        .await?;
                        return Ok(());
                    }
                };
                if subtask_specs.len() > 1 {
                    // Prepend sibling-awareness context to each subtask prompt so parallel
                    // agents know what other top-level tasks are running on the same project.
                    let siblings = store.list_siblings(&project_root, &id);
                    if !siblings.is_empty() {
                        let sibling_tasks: Vec<harness_core::prompts::SiblingTask> = siblings
                            .into_iter()
                            .filter_map(|s| {
                                s.description.and_then(|description| {
                                    if description.is_empty() {
                                        None
                                    } else {
                                        Some(harness_core::prompts::SiblingTask {
                                            issue: s.issue,
                                            description,
                                        })
                                    }
                                })
                            })
                            .collect();
                        let ctx = harness_core::prompts::sibling_task_context(&sibling_tasks);
                        subtask_specs = subtask_specs
                            .into_iter()
                            .map(|mut spec| {
                                spec.prompt = format!("{ctx}\n\n{}", spec.prompt);
                                spec
                            })
                            .collect();
                    }
                    let project_config = harness_core::config::project::load_project_config(
                        &project_root,
                    )
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "failed to load project config for {}: {e}",
                            project_root.display()
                        )
                    })?;
                    let remote = project_config.git.remote.clone();
                    let base_branch = project_config.git.base_branch.clone();

                    // Register subtask IDs in the parent before dispatch.
                    let sub_ids: Vec<TaskId> = (0..subtask_specs.len())
                        .map(|i| harness_core::types::TaskId(format!("{}-p{i}", id.0)))
                        .collect();
                    mutate_and_persist(&store, &id, |s| {
                        s.subtask_ids = sub_ids.clone();
                    })
                    .await?;

                    let context_items = crate::task_executor::helpers::collect_context_items(
                        &skills,
                        &project_root,
                        req.prompt.as_deref().unwrap_or_default(),
                    )
                    .await;

                    let turn_timeout = effective_turn_timeout(req.turn_timeout_secs);
                    let run_result = crate::parallel_dispatch::run_parallel_subtasks(
                        &id,
                        agent.clone(),
                        subtask_specs,
                        wmgr.clone(),
                        &project_root,
                        &remote,
                        &base_branch,
                        context_items,
                        turn_timeout,
                    )
                    .await;

                    // Both sequential and parallel tasks require ALL subtasks to succeed.
                    // With up to MAX_PARALLEL (8) chunks, any() would mark Done on 1/8
                    // success, silently dropping failed work.
                    let succeeded = run_result.results.iter().all(|r| r.response.is_some());
                    mutate_and_persist(&store, &id, |s| {
                        for r in &run_result.results {
                            let detail = if let Some(ref resp) = r.response {
                                if resp.output.is_empty() {
                                    None
                                } else {
                                    Some(resp.output.clone())
                                }
                            } else {
                                r.error.clone()
                            };
                            s.rounds.push(RoundResult {
                                turn: (r.index as u32).saturating_add(1),
                                action: format!("parallel_subtask_{}", r.index),
                                result: if r.response.is_some() {
                                    "success".into()
                                } else {
                                    "failed".into()
                                },
                                detail,
                                first_token_latency_ms: None,
                            });
                        }
                        if succeeded {
                            s.status = TaskStatus::Done;
                        } else {
                            let failed_count = run_result
                                .results
                                .iter()
                                .filter(|r| r.response.is_none())
                                .count();
                            let total_count = run_result.results.len();
                            s.status = TaskStatus::Failed;
                            s.error = Some(if run_result.is_sequential {
                                format!(
                                    "{}/{} sequential subtasks failed; remaining steps were skipped",
                                    failed_count, total_count
                                )
                            } else {
                                format!(
                                    "{}/{} parallel subtasks failed",
                                    failed_count, total_count
                                )
                            });
                        }
                    })
                    .await?;
                    return Ok(());
                }
            }
        }

        // Planning gate: complex prompt-only tasks must go through the Plan phase
        // before implementation to reduce drift.
        // Heuristic: prompt longer than 200 words OR containing 3+ file path tokens.
        if req.issue.is_none()
            && req.pr.is_none()
            && !is_non_decomposable_prompt_source(req.source.as_deref())
        {
            if let Some(ref prompt) = req.prompt {
                if prompt_requires_plan(prompt) {
                    mutate_and_persist(&store, &id, |s| {
                        if s.phase == TaskPhase::Implement {
                            s.phase = TaskPhase::Plan;
                        }
                    })
                    .await?;
                    tracing::info!(
                        task_id = %id.0,
                        "planning gate: complex prompt — forcing Plan phase before Implement"
                    );
                }
            }
        }

        // If workspace isolation is configured, create a per-task git worktree.
        let run_project = if let Some(ref wmgr) = workspace_mgr {
            let project_config = harness_core::config::project::load_project_config(&project_root)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to load project config for {}: {e}",
                        project_root.display()
                    )
                })?;
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

        // Retry loop: on transient errors, back off and retry up to MAX_TRANSIENT_RETRIES.
        // Retrying inside the spawn means the stream stays open and the task actually re-runs.
        let mut transient_attempts = 0u32;
        // Track total turns used across all transient-retry attempts so the
        // max_turns budget is enforced globally over the full task lifecycle.
        let mut total_turns_used: u32 = 0;
        let task_result = loop {
            // Wait if global rate-limit circuit breaker is active (another task hit the limit).
            store.wait_for_rate_limit().await;

            let result = crate::task_executor::run_task(
                &store,
                &id,
                agent.as_ref(),
                reviewer.as_deref(),
                skills.clone(),
                events.clone(),
                interceptors.clone(),
                &req,
                run_project.clone(),
                server_config.as_ref(),
                &mut total_turns_used,
            )
            .await;

            match result {
                ok @ Ok(()) => break ok,
                Err(ref e)
                    if is_transient_error(&format!("{:#}", e))
                        && transient_attempts < MAX_TRANSIENT_RETRIES =>
                {
                    transient_attempts += 1;
                    let reason = format!("{:#}", e);

                    // Account-level limit: activate global circuit breaker (60 min pause)
                    // so all other tasks stop burning turns.
                    let backoff_secs = if reason.to_lowercase().contains(ACCOUNT_LIMIT_PATTERN) {
                        let pause = std::time::Duration::from_secs(3600);
                        store.set_rate_limit(pause).await;
                        3600u64
                    } else {
                        30u64 * (1u64 << (transient_attempts - 1).min(4))
                    };

                    tracing::warn!(
                        task_id = %id.0,
                        attempt = transient_attempts,
                        max = MAX_TRANSIENT_RETRIES,
                        backoff_secs,
                        "transient failure detected; retrying after backoff: {reason}"
                    );
                    log_task_failure_event(
                        &events,
                        &id,
                        &format!("transient (attempt {transient_attempts}): {reason}"),
                    )
                    .await;
                    if let Err(pe) = mutate_and_persist(&store, &id, |s| {
                        s.rounds.push(RoundResult {
                            turn: s.turn,
                            action: "transient_retry".into(),
                            result: format!(
                                "attempt {transient_attempts}/{MAX_TRANSIENT_RETRIES}"
                            ),
                            detail: Some(reason.clone()),
                            first_token_latency_ms: None,
                        });
                        s.status = TaskStatus::Pending;
                        s.error = Some(format!(
                            "retrying after transient failure (attempt {transient_attempts}): {reason}"
                        ));
                    })
                    .await
                    {
                        tracing::error!("failed to record retry for task {id:?}: {pe}");
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                }
                err @ Err(_) => break err,
            }
        };

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

    // Store abort handle before the watcher consumes the JoinHandle, so the
    // cancel endpoint can abort the task's Tokio future (which also kills the
    // child process via kill_on_drop(true)).
    store_for_abort.store_abort_handle(&id_for_abort, handle.abort_handle());

    tokio::spawn(async move {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, e.to_string())
                    .await;
            }
            Err(join_err) if join_err.is_cancelled() => {
                // abort() was called by the cancel endpoint; status already set to
                // Cancelled before abort() was called — do not overwrite with Failed.
                tracing::info!("task {id_watcher:?} cancelled via abort");
            }
            Err(join_err) => {
                tracing::error!("task {id_watcher:?} panicked: {join_err}");
                let reason = format!("task failed unexpectedly: {join_err}");
                record_task_failure(&store_watcher, &events_watcher, &id_watcher, reason).await;
            }
        }
        store_watcher.remove_abort_handle(&id_watcher);
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
) -> anyhow::Result<()> {
    let status_str = status.as_ref().to_string();
    mutate_and_persist(store, task_id, |s| {
        s.status = status;
        s.turn = turn;
    })
    .await?;
    store.log_event(crate::event_replay::TaskEvent::StatusChanged {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        status: status_str,
        turn,
    });
    Ok(())
}

/// Mutate a task in the cache then persist to SQLite.
pub(crate) async fn mutate_and_persist(
    store: &TaskStore,
    id: &TaskId,
    f: impl FnOnce(&mut TaskState),
) -> anyhow::Result<()> {
    if let Some(mut entry) = store.cache.get_mut(id) {
        f(entry.value_mut());
    }
    store.persist(id).await
}

// --- dependency scheduling ---

/// DFS cycle detection. Returns true if any dependency transitively reaches `new_id`.
/// Called before the new task is inserted into the store.
fn detect_cycle(store: &TaskStore, new_id: &TaskId, depends_on: &[TaskId]) -> bool {
    let mut stack: Vec<TaskId> = depends_on.to_vec();
    let mut visited: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
    while let Some(dep_id) = stack.pop() {
        if dep_id == *new_id {
            return true;
        }
        if visited.contains(&dep_id) {
            continue;
        }
        visited.insert(dep_id.clone());
        if let Some(dep_state) = store.get(&dep_id) {
            for transitive in &dep_state.depends_on {
                stack.push(transitive.clone());
            }
        }
    }
    false
}

/// Create a task that waits for its dependencies before starting.
///
/// If all deps are already Done, registers as Pending immediately.
/// If deps are unresolved, registers as AwaitingDeps.
/// Returns Err if a circular dependency is detected.
pub async fn spawn_task_awaiting_deps(
    store: Arc<TaskStore>,
    req: CreateTaskRequest,
) -> anyhow::Result<TaskId> {
    let depends_on = req.depends_on.clone();
    let task_id = TaskId::new();

    if detect_cycle(&store, &task_id, &depends_on) {
        anyhow::bail!("circular dependency detected for task {}", task_id.0);
    }

    let mut all_done = true;
    for dep_id in &depends_on {
        if !matches!(store.dep_status(dep_id).await, Some(TaskStatus::Done)) {
            all_done = false;
            break;
        }
    }

    let mut state = TaskState::new(task_id.clone());
    state.depends_on = depends_on;
    state.source = req.source.clone();
    state.external_id = req.external_id.clone();
    state.repo = req.repo.clone();
    state.priority = req.priority;
    if let Some(parent) = req.parent_task_id.clone() {
        state.parent_id = Some(parent);
    }

    if !all_done && !state.depends_on.is_empty() {
        state.status = TaskStatus::AwaitingDeps;
    }

    store.insert(&state).await;
    store.register_task_stream(&task_id);
    Ok(task_id)
}

/// Check all AwaitingDeps tasks and transition ready ones to Pending.
/// Returns the IDs of tasks that were transitioned to Pending.
///
/// Async because dependency status checks fall back to the database for
/// terminal tasks (Done/Failed) that were evicted from the startup cache.
///
/// Returns `(ready_ids, newly_failed_ids)`.  Both sets must be persisted by
/// the caller so that status transitions survive a process restart.
pub async fn check_awaiting_deps(store: &TaskStore) -> (Vec<TaskId>, Vec<TaskId>) {
    let mut ready = Vec::new();
    let mut failed_deps: Vec<(TaskId, TaskId)> = Vec::new();

    // Snapshot AwaitingDeps tasks from cache before async work.
    let awaiting: Vec<(TaskId, Vec<TaskId>)> = store
        .cache
        .iter()
        .filter_map(|e| {
            let task = e.value();
            if matches!(task.status, TaskStatus::AwaitingDeps) {
                Some((task.id.clone(), task.depends_on.clone()))
            } else {
                None
            }
        })
        .collect();

    for (task_id, depends_on) in awaiting {
        // Single pass: one DB lookup per dep (cache-first, then lightweight
        // `SELECT status` — no `rounds` JSON decode).
        let mut failed_dep_id = None;
        let mut all_done = true;
        for dep_id in &depends_on {
            match store.dep_status(dep_id).await {
                Some(TaskStatus::Failed) => {
                    failed_dep_id = Some(dep_id.clone());
                    break; // fail-fast — no need to inspect remaining deps
                }
                Some(TaskStatus::Done) => {} // this dep is satisfied
                _ => {
                    // Pending, Implementing, or unknown — not ready yet.
                    // Keep scanning in case a later dep is Failed.
                    all_done = false;
                }
            }
        }
        if let Some(fd) = failed_dep_id {
            failed_deps.push((task_id, fd));
            continue;
        }
        if all_done {
            ready.push(task_id);
        }
    }

    let mut newly_failed: Vec<TaskId> = Vec::new();
    for (task_id, failed_dep_id) in &failed_deps {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            // Only overwrite if the task is still AwaitingDeps — a concurrent
            // cancel or status transition must not be clobbered.
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Failed;
                entry.error = Some(format!("dependency {} failed", failed_dep_id.0));
                newly_failed.push(task_id.clone());
            }
        }
    }
    for task_id in &ready {
        if let Some(mut entry) = store.cache.get_mut(task_id) {
            // Same guard: skip if status changed since we snapshotted.
            if matches!(entry.status, TaskStatus::AwaitingDeps) {
                entry.status = TaskStatus::Pending;
            }
        }
    }

    (ready, newly_failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use harness_core::agent::{AgentRequest, AgentResponse, StreamItem};
    use harness_core::types::{Capability, ContextItem, EventFilters, ExecutionPhase, TokenUsage};
    use tokio::time::Duration;

    #[tokio::test]
    async fn task_stream_subscribe_and_publish() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = harness_core::types::TaskId("stream-test".to_string());

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
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let id = harness_core::types::TaskId("backpressure-test".to_string());

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
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let parent_id = harness_core::types::TaskId("parent-task".to_string());
        let parent = TaskState::new(parent_id.clone());
        store.insert(&parent).await;

        let mut child1 = TaskState::new(harness_core::types::TaskId("child-1".to_string()));
        child1.parent_id = Some(parent_id.clone());
        store.insert(&child1).await;

        let mut child2 = TaskState::new(harness_core::types::TaskId("child-2".to_string()));
        child2.parent_id = Some(parent_id.clone());
        store.insert(&child2).await;

        // Unrelated task.
        store
            .insert(&TaskState::new(harness_core::types::TaskId(
                "other".to_string(),
            )))
            .await;

        let children = store.list_children(&parent_id);
        assert_eq!(children.len(), 2);
        assert!(children
            .iter()
            .all(|c| c.parent_id.as_ref() == Some(&parent_id)));

        let no_children =
            store.list_children(&harness_core::types::TaskId("nonexistent".to_string()));
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
        assert!(state.project_root.is_none());
        assert!(state.issue.is_none());
        assert!(state.description.is_none());
    }

    #[tokio::test]
    async fn list_siblings_returns_active_tasks_for_same_project() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let project = PathBuf::from("/repo/project");
        let other_project = PathBuf::from("/repo/other");

        let current_id = harness_core::types::TaskId("current".to_string());
        let mut current = TaskState::new(current_id.clone());
        current.project_root = Some(project.clone());
        current.status = TaskStatus::Implementing;
        store.insert(&current).await;

        // Sibling on same project in Implementing status.
        let mut sibling1 = TaskState::new(harness_core::types::TaskId("sibling-1".to_string()));
        sibling1.project_root = Some(project.clone());
        sibling1.status = TaskStatus::Implementing;
        sibling1.issue = Some(77);
        sibling1.description = Some("fix unwrap in s3.rs".to_string());
        store.insert(&sibling1).await;

        // Sibling on same project in Pending status.
        let mut sibling2 = TaskState::new(harness_core::types::TaskId("sibling-2".to_string()));
        sibling2.project_root = Some(project.clone());
        sibling2.status = TaskStatus::Pending;
        store.insert(&sibling2).await;

        // Task on a different project — must not appear.
        let mut other = TaskState::new(harness_core::types::TaskId("other-project".to_string()));
        other.project_root = Some(other_project.clone());
        other.status = TaskStatus::Implementing;
        store.insert(&other).await;

        // Done task on same project — must not appear.
        let mut done = TaskState::new(harness_core::types::TaskId("done-task".to_string()));
        done.project_root = Some(project.clone());
        done.status = TaskStatus::Done;
        store.insert(&done).await;

        let siblings = store.list_siblings(&project, &current_id);
        let sibling_ids: Vec<&str> = siblings.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(
            siblings.len(),
            2,
            "expected 2 siblings, got: {sibling_ids:?}"
        );
        assert!(
            siblings.iter().all(|s| s.id != current_id),
            "current task must be excluded"
        );
        assert!(siblings
            .iter()
            .all(|s| s.project_root.as_deref() == Some(project.as_path())));

        // One sibling on `other_project`.
        let other_project_siblings = store.list_siblings(&other_project, &current_id);
        assert_eq!(other_project_siblings.len(), 1);

        Ok(())
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
    impl harness_core::agent::CodeAgent for CapturingAgent {
        fn name(&self) -> &str {
            "capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
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
        ) -> harness_core::error::Result<()> {
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
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut skill_store = harness_skills::store::SkillStore::new();
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
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
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
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> =
            vec![Arc::new(BlockingInterceptor)];

        let req = CreateTaskRequest {
            prompt: Some("blocked task".into()),
            issue: None,
            pr: None,
            agent: None,
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
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

        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;
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

    /// Verify that a local u32 counter correctly tracks waiting rounds without any store query.
    /// Task execution is sequential within a single tokio task, so a plain local counter suffices.
    #[test]
    fn local_waiting_counter_increments_on_each_waiting_response() {
        let max_rounds = 5u32;
        let mut waiting_count: u32 = 0;
        let mut observed: Vec<u32> = Vec::new();

        // Simulate the initial wait before the review loop.
        waiting_count += 1;
        observed.push(waiting_count);

        // Simulate inter-round waits (max_rounds - 1 additional waits).
        for _ in 1..max_rounds {
            waiting_count += 1;
            observed.push(waiting_count);
        }

        let expected: Vec<u32> = (1..=max_rounds).collect();
        assert_eq!(
            observed, expected,
            "waiting_count must increment monotonically on each waiting response"
        );
    }

    #[tokio::test]
    async fn spawn_blocking_panic_surfaces_error_and_event() -> anyhow::Result<()> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let dir = tempfile::Builder::new()
            .prefix("harness-test-")
            .tempdir_in(&home)?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let agent = CapturingAgent::new();
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        let req = CreateTaskRequest {
            prompt: Some("panic path".into()),
            issue: None,
            pr: None,
            agent: None,
            project: None,
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            max_budget_usd: None,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
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
            None,
            None,
        )
        .await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;
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

    #[tokio::test]
    async fn register_pending_task_keeps_repo_metadata_visible() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let req = CreateTaskRequest {
            issue: Some(20),
            source: Some("github".to_string()),
            external_id: Some("20".to_string()),
            repo: Some("acme/harness".to_string()),
            ..Default::default()
        };

        let task_id = register_pending_task(store.clone(), &req).await;
        let state = store.get(&task_id).ok_or_else(|| {
            anyhow::anyhow!("task not found in store — possible concurrent deletion")
        })?;

        assert_eq!(state.source.as_deref(), Some("github"));
        assert_eq!(state.external_id.as_deref(), Some("20"));
        assert_eq!(state.repo.as_deref(), Some("acme/harness"));
        assert_eq!(state.description.as_deref(), Some("issue #20"));
        Ok(())
    }

    /// Mock agent that records the `execution_phase` from every call and
    /// returns pre-configured responses in order.
    struct PhaseCapturingAgent {
        phases: tokio::sync::Mutex<Vec<Option<ExecutionPhase>>>,
        responses: tokio::sync::Mutex<Vec<String>>,
    }

    impl PhaseCapturingAgent {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                phases: tokio::sync::Mutex::new(Vec::new()),
                responses: tokio::sync::Mutex::new(responses),
            })
        }

        async fn captured_phases(&self) -> Vec<Option<ExecutionPhase>> {
            self.phases.lock().await.clone()
        }

        async fn next_response(&self) -> String {
            let mut guard = self.responses.lock().await;
            if guard.is_empty() {
                String::new()
            } else {
                guard.remove(0)
            }
        }
    }

    #[async_trait]
    impl harness_core::agent::CodeAgent for PhaseCapturingAgent {
        fn name(&self) -> &str {
            "phase-capturing-mock"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![]
        }

        async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            self.phases.lock().await.push(req.execution_phase);
            let output = self.next_response().await;
            Ok(AgentResponse {
                output,
                stderr: String::new(),
                items: vec![],
                token_usage: TokenUsage::default(),
                model: "mock".into(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            req: AgentRequest,
            tx: tokio::sync::mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.phases.lock().await.push(req.execution_phase);
            let output = self.next_response().await;
            if !output.is_empty() {
                if let Err(e) = tx.send(StreamItem::MessageDelta { text: output }).await {
                    tracing::warn!("PhaseCapturingAgent: failed to send MessageDelta: {e}");
                }
            }
            if let Err(e) = tx.send(StreamItem::Done).await {
                tracing::warn!("PhaseCapturingAgent: failed to send Done: {e}");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn planning_phase_is_set_on_initial_implementation_turn() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        // Agent returns empty output (no PR URL) → task completes after implementation.
        let agent = PhaseCapturingAgent::new(vec![String::new()]);
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("implement something".into()),
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(0),
            turn_timeout_secs: 30,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
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

        tokio::time::sleep(Duration::from_millis(300)).await;

        let phases = agent.captured_phases().await;
        assert!(
            !phases.is_empty(),
            "expected at least one agent call, got none"
        );
        assert_eq!(
            phases[0],
            Some(ExecutionPhase::Planning),
            "initial implementation turn must use Planning phase"
        );
        Ok(())
    }

    #[tokio::test]
    async fn validation_phase_is_set_on_review_loop_turns() -> anyhow::Result<()> {
        let _lock = crate::test_helpers::HOME_LOCK.lock().await;
        let dir = crate::test_helpers::tempdir_in_home("harness-test-")?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        let skills = Arc::new(RwLock::new(harness_skills::store::SkillStore::new()));
        let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);

        // Call 1 (execute_stream): return a PR URL to trigger the review loop.
        // Call 2 (execute): return LGTM to complete the review loop.
        let agent = PhaseCapturingAgent::new(vec![
            "PR_URL=https://github.com/owner/repo/pull/1".into(),
            "LGTM".into(),
        ]);
        let agent_clone = agent.clone();

        let req = CreateTaskRequest {
            prompt: Some("implement something".into()),
            project: Some(dir.path().to_path_buf()),
            wait_secs: 0,
            max_rounds: Some(1),
            turn_timeout_secs: 30,
            ..Default::default()
        };

        let queue = crate::task_queue::TaskQueue::unbounded();
        let permit = queue.acquire("test", 0).await?;
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

        tokio::time::sleep(Duration::from_millis(300)).await;

        let phases = agent.captured_phases().await;
        assert!(
            phases.len() >= 2,
            "expected at least 2 agent calls (implementation + review check), got {}",
            phases.len()
        );
        assert_eq!(
            phases[0],
            Some(ExecutionPhase::Planning),
            "implementation turn must use Planning phase"
        );
        assert_eq!(
            phases[1],
            Some(ExecutionPhase::Execution),
            "review loop turn must use Execution phase (agent needs write access to fix bot comments)"
        );
        Ok(())
    }

    #[test]
    fn transient_error_detection() {
        // Positive cases — should match transient patterns.
        assert!(is_transient_error(
            "agent execution failed: claude exited with exit status: 1: Selected model is at capacity"
        ));
        assert!(is_transient_error("rate limit exceeded, retry after 30s"));
        assert!(is_transient_error("HTTP 429 Too Many Requests"));
        assert!(is_transient_error("502 Bad Gateway"));
        assert!(is_transient_error(
            "Agent stream stalled: no output for 300s"
        ));
        assert!(is_transient_error("connection reset by peer"));
        assert!(is_transient_error(
            "stream idle timeout after 300s: zombie connection terminated"
        ));
        assert!(is_transient_error(
            "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
        ));

        // Negative cases — permanent errors should not match.
        assert!(!is_transient_error(
            "Task did not receive LGTM after 5 review rounds."
        ));
        assert!(!is_transient_error(
            "triage output unparseable — agent did not produce TRIAGE=<decision>"
        ));
        assert!(!is_transient_error("all parallel subtasks failed"));
        assert!(!is_transient_error(
            "task failed unexpectedly: task 102 panicked"
        ));
        assert!(!is_transient_error(
            "budget exceeded: spent $5.00, limit $3.00"
        ));
    }

    #[test]
    fn parse_pr_url_standard() {
        let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/42")
        else {
            panic!("expected Some for standard GitHub PR URL");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 42);
    }

    #[test]
    fn parse_pr_url_with_fragment() {
        let Some((owner, repo, number)) =
            parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
        else {
            panic!("expected Some for PR URL with fragment");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 99);
    }

    #[test]
    fn parse_pr_url_trailing_slash() {
        let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/7/")
        else {
            panic!("expected Some for PR URL with trailing slash");
        };
        assert_eq!(owner, "acme");
        assert_eq!(repo, "myrepo");
        assert_eq!(number, 7);
    }

    #[test]
    fn parse_pr_url_invalid_returns_none() {
        assert!(parse_pr_url("https://github.com/acme/myrepo").is_none());
        assert!(parse_pr_url("not-a-url").is_none());
        assert!(parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
    }

    // --- planning gate ---

    #[test]
    fn short_prompt_does_not_require_plan() {
        let prompt = "Fix the typo in README.md";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn long_prompt_over_200_words_requires_plan() {
        let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
        let prompt = words.join(" ");
        assert!(prompt_requires_plan(&prompt));
    }

    #[test]
    fn prompt_with_three_file_paths_requires_plan() {
        let prompt =
            "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
        assert!(prompt_requires_plan(prompt));
    }

    #[test]
    fn prompt_with_two_file_paths_does_not_require_plan() {
        let prompt = "Update src/foo.rs and crates/bar/src/lib.rs to fix X";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn xml_closing_tags_are_not_counted_as_file_paths() {
        // `wrap_external_data` wraps content in <external_data>...</external_data>.
        // Three `</external_data>` closing tags contain '/' but must NOT trigger
        // the planning gate — they are markup, not file paths.
        let prompt = "GC applied files:\n\
                      <external_data>test-guard.sh</external_data>\n\
                      Rationale:\n<external_data>test</external_data>\n\
                      Validation:\n<external_data>test</external_data>";
        assert!(!prompt_requires_plan(prompt));
    }

    #[test]
    fn non_decomposable_source_list_includes_periodic_and_sprint() {
        assert!(is_non_decomposable_prompt_source(Some("periodic_review")));
        assert!(is_non_decomposable_prompt_source(Some("sprint_planner")));
        assert!(!is_non_decomposable_prompt_source(Some("github")));
        assert!(!is_non_decomposable_prompt_source(None));
    }

    #[test]
    fn task_status_semantics_are_centralized() {
        let cases = [
            (
                TaskStatus::Pending,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            (
                TaskStatus::AwaitingDeps,
                false,
                false,
                false,
                false,
                false,
                false,
            ),
            (
                TaskStatus::Implementing,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (
                TaskStatus::AgentReview,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (TaskStatus::Waiting, false, true, true, false, false, false),
            (
                TaskStatus::Reviewing,
                false,
                true,
                true,
                false,
                false,
                false,
            ),
            (TaskStatus::Done, true, false, false, true, false, false),
            (TaskStatus::Failed, true, false, false, false, true, false),
            (
                TaskStatus::Cancelled,
                true,
                false,
                false,
                false,
                false,
                true,
            ),
        ];

        for (status, terminal, inflight, resumable, success, failure, cancelled) in cases {
            assert_eq!(status.is_terminal(), terminal, "{status:?} terminal");
            assert_eq!(status.is_inflight(), inflight, "{status:?} inflight");
            assert_eq!(
                status.is_resumable_after_restart(),
                resumable,
                "{status:?} resumable"
            );
            assert_eq!(status.is_success(), success, "{status:?} success");
            assert_eq!(status.is_failure(), failure, "{status:?} failure");
            assert_eq!(status.is_cancelled(), cancelled, "{status:?} cancelled");
        }

        assert_eq!(
            TaskStatus::terminal_statuses(),
            &["done", "failed", "cancelled"]
        );
        assert_eq!(
            TaskStatus::resumable_statuses(),
            &["implementing", "agent_review", "waiting", "reviewing"]
        );
    }

    #[tokio::test]
    async fn count_by_project_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
        assert!(store.count_for_dashboard().await.by_project.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_none_root_excluded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let mut task = TaskState::new(harness_core::types::TaskId("no-root".to_string()));
        task.status = TaskStatus::Done;
        // project_root stays None
        store.insert(&task).await;

        assert!(
            store.count_for_dashboard().await.by_project.is_empty(),
            "tasks with no project_root must not appear in per-project counts"
        );
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_groups_correctly() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let root_a = std::path::PathBuf::from("/projects/alpha");
        let root_b = std::path::PathBuf::from("/projects/beta");

        for (id, root, status) in [
            ("a1", &root_a, TaskStatus::Done),
            ("a2", &root_a, TaskStatus::Done),
            ("a3", &root_a, TaskStatus::Failed),
            ("a4", &root_a, TaskStatus::Cancelled),
            ("b1", &root_b, TaskStatus::Done),
            ("b2", &root_b, TaskStatus::Failed),
            ("b3", &root_b, TaskStatus::Failed),
            ("b4", &root_b, TaskStatus::Cancelled),
        ] {
            let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            store.insert(&task).await;
        }

        let counts = store.count_for_dashboard().await.by_project;
        let key_a = root_a.to_string_lossy().into_owned();
        let key_b = root_b.to_string_lossy().into_owned();

        assert!(counts.contains_key(&key_a), "alpha counts missing");
        assert_eq!(counts[&key_a].done, 2, "alpha done");
        assert_eq!(counts[&key_a].failed, 1, "alpha failed");

        assert!(counts.contains_key(&key_b), "beta counts missing");
        assert_eq!(counts[&key_b].done, 1, "beta done");
        assert_eq!(counts[&key_b].failed, 2, "beta failed");
        Ok(())
    }

    #[tokio::test]
    async fn count_by_project_excludes_cancelled_from_failed_totals() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("tasks.db")).await?;

        let root = std::path::PathBuf::from("/projects/alpha");
        for (id, status) in [
            ("done", TaskStatus::Done),
            ("failed", TaskStatus::Failed),
            ("cancelled", TaskStatus::Cancelled),
        ] {
            let mut task = TaskState::new(harness_core::types::TaskId(id.to_string()));
            task.status = status;
            task.project_root = Some(root.clone());
            store.insert(&task).await;
        }

        let counts = store.count_for_dashboard().await;
        assert_eq!(counts.global_done, 1);
        assert_eq!(counts.global_failed, 1);
        let key = root.to_string_lossy().into_owned();
        assert_eq!(counts.by_project[&key].done, 1);
        assert_eq!(counts.by_project[&key].failed, 1);
        Ok(())
    }
}
