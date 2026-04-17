use harness_core::types::TaskId as CoreTaskId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

pub type TaskId = CoreTaskId;

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
    /// Additional caller-supplied prompt context for issue tasks.
    ///
    /// Callers may submit `{ issue: N, prompt: "extra context" }` to augment
    /// issue resolution.  The prompt is not stored in `description` (privacy),
    /// so we persist it here for issue tasks only to survive a server restart.
    /// Pure prompt tasks and PR tasks leave this `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_prompt: Option<String>,
    /// Primary prompt for prompt-only tasks (no issue or pr).
    ///
    /// Kept in memory so that `AwaitingDeps` tasks can reconstruct the original
    /// request when their dependencies resolve within the same server session.
    /// Intentionally NOT serialised to the database: prompts may contain
    /// credentials or sensitive data and must not be written at rest.
    /// LIMITATION: after a server restart the prompt will be absent; prompt-only
    /// `AwaitingDeps` tasks will transition to `Pending` once their dependencies
    /// resolve, then fail immediately in the dep-watcher dispatch path because
    /// no prompt is available to replay.
    #[serde(skip)]
    pub prompt: Option<String>,
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
            // Only persist the caller's prompt for issue tasks — it serves as
            // additional context that is safe to store (unlike pure prompt
            // tasks which may contain credentials).  PR and prompt-only tasks
            // leave this None.
            additional_prompt: if req.issue.is_some() {
                req.prompt.clone()
            } else {
                None
            },
            // For prompt-only tasks (no issue/pr), store the prompt so that
            // AwaitingDeps tasks can reconstruct the original request when deps resolve.
            prompt: if req.issue.is_none() && req.pr.is_none() {
                req.prompt.clone()
            } else {
                None
            },
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
        // Restore the additional prompt context for recovered issue tasks.
        if self.additional_prompt.is_some() {
            req.prompt = self.additional_prompt.clone();
        }
        // Restore the primary prompt for recovered prompt-only tasks.
        if self.prompt.is_some() {
            req.prompt = self.prompt.clone();
        }
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

fn default_stall_timeout() -> u64 {
    300
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
        assert!(state.project_root.is_none());
        assert!(state.issue.is_none());
        assert!(state.description.is_none());
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
}
