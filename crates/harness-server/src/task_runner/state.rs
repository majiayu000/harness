use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::request::{PersistedRequestSettings, SystemTaskInput};
use super::types::{TaskFailureKind, TaskId, TaskKind, TaskPhase, TaskStatus};

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
    pub task_kind: TaskKind,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<TaskFailureKind>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    /// Workspace path used for isolated execution. Persisted for observability and
    /// deterministic stale-workspace reconciliation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<PathBuf>,
    /// Stable server/session token that currently owns the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_owner: Option<String>,
    /// Monotonic execution generation for this task. Incremented before each
    /// workspace admission so stale ownership can be reconciled deterministically.
    #[serde(default)]
    pub run_generation: u32,
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
    /// ISO 8601 last-updated timestamp. Populated when loaded from the DB.
    #[serde(default)]
    pub updated_at: Option<String>,
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
    /// Restart-safe prompt snapshot for trusted system-generated prompt tasks.
    /// Persisted internally for recovery only; never expose it via the public task API.
    #[serde(skip)]
    pub system_input: Option<SystemTaskInput>,
}

/// Lightweight task summary returned by the list endpoint (excludes `rounds` history).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub task_kind: TaskKind,
    pub status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<TaskFailureKind>,
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
    /// Persisted workspace path for lifecycle diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Persisted workspace owner token for lifecycle diagnostics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_owner: Option<String>,
    /// Monotonic execution generation for this task.
    #[serde(default)]
    pub run_generation: u32,
    /// Issue workflow state summary, when this task belongs to an issue workflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<harness_workflow::issue_lifecycle::IssueWorkflowInstance>,
}

/// Lightweight recent-failure row used by the operator snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentFailureTask {
    pub id: TaskId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<TaskFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_owner: Option<String>,
    #[serde(default)]
    pub run_generation: u32,
    pub error: Option<String>,
    #[serde(default)]
    pub failed_at: Option<String>,
}

impl TaskState {
    pub fn effective_failure_kind(&self) -> Option<TaskFailureKind> {
        self.failure_kind.clone().or_else(|| {
            if self.status.is_failure() {
                Some(TaskFailureKind::Task)
            } else {
                None
            }
        })
    }

    pub(crate) fn new(id: TaskId) -> Self {
        Self {
            id,
            task_kind: TaskKind::default(),
            status: TaskStatus::Pending,
            failure_kind: None,
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
            workspace_path: None,
            workspace_owner: None,
            run_generation: 0,
            issue: None,
            description: None,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            updated_at: None,
            priority: 0,
            phase: TaskPhase::default(),
            triage_output: None,
            plan_output: None,
            repo: None,
            request_settings: None,
            system_input: None,
        }
    }

    pub fn summary(&self) -> TaskSummary {
        TaskSummary {
            id: self.id.clone(),
            task_kind: self.task_kind,
            status: self.status.clone(),
            failure_kind: self.effective_failure_kind(),
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
            workspace_path: self
                .workspace_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            workspace_owner: self.workspace_owner.clone(),
            run_generation: self.run_generation,
            workflow: None,
        }
    }
}
