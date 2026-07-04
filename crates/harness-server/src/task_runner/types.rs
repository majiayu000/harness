use harness_core::types::TaskId as CoreTaskId;
use serde::{Deserialize, Serialize};

pub type TaskId = CoreTaskId;

pub const ROUND_BUDGET_EXHAUSTED_REASON: &str = "round_budget_exhausted";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Issue,
    Pr,
    #[default]
    Prompt,
    Review,
    Planner,
}

impl TaskKind {
    pub fn classify(source: Option<&str>, issue: Option<u64>, pr: Option<u64>) -> Self {
        match source {
            Some("periodic_review") => Self::Review,
            Some("sprint_planner") => Self::Planner,
            _ if issue.is_some() => Self::Issue,
            _ if pr.is_some() => Self::Pr,
            _ => Self::Prompt,
        }
    }

    pub fn from_persisted(
        persisted: Option<&str>,
        source: Option<&str>,
        external_id: Option<&str>,
        description: Option<&str>,
    ) -> anyhow::Result<Self> {
        match persisted {
            Some("legacy") | None => Ok(Self::infer_legacy(source, external_id, description)),
            Some("issue") => Ok(Self::Issue),
            Some("pr") => Ok(Self::Pr),
            Some("prompt") => Ok(Self::Prompt),
            Some("review") => Ok(Self::Review),
            Some("planner") => Ok(Self::Planner),
            Some(other) => anyhow::bail!("unknown task kind `{other}`"),
        }
    }

    fn infer_legacy(
        source: Option<&str>,
        external_id: Option<&str>,
        description: Option<&str>,
    ) -> Self {
        if external_id.is_some_and(|id| id.starts_with("issue:"))
            || description.is_some_and(|desc| desc.starts_with("issue #"))
        {
            Self::Issue
        } else if external_id.is_some_and(|id| id.starts_with("pr:"))
            || description.is_some_and(|desc| desc.starts_with("PR #"))
        {
            Self::Pr
        } else {
            match source {
                Some("periodic_review") => Self::Review,
                Some("sprint_planner") => Self::Planner,
                _ => Self::Prompt,
            }
        }
    }

    pub fn default_phase(&self) -> TaskPhase {
        match self {
            Self::Planner => TaskPhase::Plan,
            Self::Review => TaskPhase::Review,
            Self::Issue | Self::Pr | Self::Prompt => TaskPhase::Implement,
        }
    }

    pub fn execution_status(&self) -> TaskStatus {
        match self {
            Self::Review => TaskStatus::ReviewGenerating,
            Self::Planner => TaskStatus::PlannerGenerating,
            Self::Issue | Self::Pr | Self::Prompt => TaskStatus::Implementing,
        }
    }

    pub fn recovery_status(&self) -> Option<TaskStatus> {
        match self {
            Self::Review => Some(TaskStatus::ReviewWaiting),
            Self::Planner => Some(TaskStatus::PlannerWaiting),
            Self::Issue | Self::Pr | Self::Prompt => None,
        }
    }

    /// Stable, user-facing reason string emitted when a task of this kind is
    /// found with no persisted external identifier during orphan recovery.
    /// Centralised here so that adding a new variant forces an explicit
    /// orphan-reason decision in one place rather than ten.
    pub fn orphan_recovery_failure_reason(&self) -> &'static str {
        match self {
            Self::Issue => "orphaned issue task: issue number not persisted",
            Self::Pr => "orphaned PR task: PR number not persisted",
            Self::Prompt => "orphaned prompt-only task: prompt not persisted",
            Self::Review => "orphaned review task: prompt not persisted",
            Self::Planner => "orphaned planner task: prompt not persisted",
        }
    }

    /// Short human-readable label used as the persisted task description when
    /// the request carries a prompt but no structured issue / PR identifier.
    /// `None` for kinds that never receive a freeform prompt-only submission;
    /// today every kind does, but the option leaves room for future variants
    /// that should reject prompt-only intake.
    pub fn prompt_task_label(&self) -> Option<&'static str> {
        Some(match self {
            Self::Review => "periodic review",
            Self::Planner => "sprint planner",
            Self::Issue | Self::Pr | Self::Prompt => "prompt task",
        })
    }

    pub fn requires_pr_url(&self) -> bool {
        matches!(self, Self::Issue | Self::Pr)
    }

    pub fn is_non_decomposable_prompt(&self) -> bool {
        matches!(self, Self::Review | Self::Planner)
    }
}

impl AsRef<str> for TaskKind {
    fn as_ref(&self) -> &str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "pr",
            Self::Prompt => "prompt",
            Self::Review => "review",
            Self::Planner => "planner",
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    AwaitingDeps,
    /// Claude is actively running the triage agent for this task.
    Triaging,
    /// Claude is actively running the plan agent for this task.
    Planning,
    Implementing,
    ReviewGenerating,
    ReviewWaiting,
    PlannerGenerating,
    PlannerWaiting,
    AgentReview,
    Waiting,
    Reviewing,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTerminalFailure {
    pub reason: String,
    pub rounds_used: u32,
    pub last_status: TaskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
}

impl TaskTerminalFailure {
    pub fn new(
        reason: impl Into<String>,
        rounds_used: u32,
        last_status: TaskStatus,
        waiting_on: Option<String>,
    ) -> Self {
        Self {
            reason: reason.into(),
            rounds_used,
            last_status,
            waiting_on,
        }
    }

    pub fn round_budget_exhausted(
        rounds_used: u32,
        last_status: TaskStatus,
        waiting_on: Option<String>,
    ) -> Self {
        Self::new(
            ROUND_BUDGET_EXHAUSTED_REASON,
            rounds_used,
            last_status,
            waiting_on,
        )
    }

    pub fn to_reason_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| self.reason.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTerminalClassification {
    Done,
    Failed,
    Stalled,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTerminalInfo {
    pub status: TaskStatus,
    pub classification: TaskTerminalClassification,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rounds_used: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<TaskStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
}

impl TaskTerminalInfo {
    pub fn from_status_error(status: &TaskStatus, error: Option<&str>) -> Option<Self> {
        if !status.is_terminal() {
            return None;
        }
        let failure = error.and_then(parse_terminal_failure);
        let classification = match status {
            TaskStatus::Done => TaskTerminalClassification::Done,
            TaskStatus::Cancelled => TaskTerminalClassification::Cancelled,
            TaskStatus::Failed
                if failure
                    .as_ref()
                    .is_some_and(|failure| failure.reason == ROUND_BUDGET_EXHAUSTED_REASON) =>
            {
                TaskTerminalClassification::Stalled
            }
            TaskStatus::Failed => TaskTerminalClassification::Failed,
            _ => return None,
        };
        let reason = failure
            .as_ref()
            .map(|failure| failure.reason.clone())
            .or_else(|| error.map(ToOwned::to_owned));
        Some(Self {
            status: status.clone(),
            classification,
            reason,
            rounds_used: failure.as_ref().map(|failure| failure.rounds_used),
            last_status: failure.as_ref().map(|failure| failure.last_status.clone()),
            waiting_on: failure.and_then(|failure| failure.waiting_on),
        })
    }

    pub fn is_stalled(&self) -> bool {
        self.classification == TaskTerminalClassification::Stalled
    }
}

pub fn parse_terminal_failure(error: &str) -> Option<TaskTerminalFailure> {
    serde_json::from_str::<TaskTerminalFailure>(error).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskTerminalOutcome {
    Completed,
    Failed(TaskTerminalFailure),
    Cancelled(Option<String>),
}

impl TaskTerminalOutcome {
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Completed => TaskStatus::Done,
            Self::Failed(_) => TaskStatus::Failed,
            Self::Cancelled(_) => TaskStatus::Cancelled,
        }
    }

    pub fn reason_string(&self) -> Option<String> {
        match self {
            Self::Completed => None,
            Self::Failed(reason) => Some(reason.to_reason_string()),
            Self::Cancelled(reason) => reason.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFailureKind {
    Task,
    WorkspaceLifecycle,
}

const TERMINAL_TASK_STATUSES: &[&str] = &[
    TaskStatus::Done.as_str(),
    TaskStatus::Failed.as_str(),
    TaskStatus::Cancelled.as_str(),
];
const RESUMABLE_TASK_STATUSES: &[&str] = &[
    TaskStatus::Triaging.as_str(),
    TaskStatus::Planning.as_str(),
    TaskStatus::Implementing.as_str(),
    TaskStatus::ReviewGenerating.as_str(),
    TaskStatus::ReviewWaiting.as_str(),
    TaskStatus::PlannerGenerating.as_str(),
    TaskStatus::PlannerWaiting.as_str(),
    TaskStatus::AgentReview.as_str(),
    TaskStatus::Waiting.as_str(),
    TaskStatus::Reviewing.as_str(),
];

impl TaskStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::AwaitingDeps => "awaiting_deps",
            Self::Triaging => "triaging",
            Self::Planning => "planning",
            Self::Implementing => "implementing",
            Self::ReviewGenerating => "review_generating",
            Self::ReviewWaiting => "review_waiting",
            Self::PlannerGenerating => "planner_generating",
            Self::PlannerWaiting => "planner_waiting",
            Self::AgentReview => "agent_review",
            Self::Waiting => "waiting",
            Self::Reviewing => "reviewing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    pub fn is_inflight(&self) -> bool {
        matches!(
            self,
            Self::Triaging
                | Self::Planning
                | Self::Implementing
                | Self::ReviewGenerating
                | Self::ReviewWaiting
                | Self::PlannerGenerating
                | Self::PlannerWaiting
                | Self::AgentReview
                | Self::Waiting
                | Self::Reviewing
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
        self.as_str()
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(TaskStatus::Pending),
            "awaiting_deps" => Ok(TaskStatus::AwaitingDeps),
            "triaging" => Ok(TaskStatus::Triaging),
            "planning" => Ok(TaskStatus::Planning),
            "implementing" => Ok(TaskStatus::Implementing),
            "review_generating" => Ok(TaskStatus::ReviewGenerating),
            "review_waiting" => Ok(TaskStatus::ReviewWaiting),
            "planner_generating" => Ok(TaskStatus::PlannerGenerating),
            "planner_waiting" => Ok(TaskStatus::PlannerWaiting),
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

impl AsRef<str> for TaskFailureKind {
    fn as_ref(&self) -> &str {
        match self {
            TaskFailureKind::Task => "task",
            TaskFailureKind::WorkspaceLifecycle => "workspace_lifecycle",
        }
    }
}

impl std::str::FromStr for TaskFailureKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task" => Ok(TaskFailureKind::Task),
            "workspace_lifecycle" => Ok(TaskFailureKind::WorkspaceLifecycle),
            _ => anyhow::bail!("unknown task failure kind `{s}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TaskKind;

    #[test]
    fn legacy_issue_markers_override_review_source() {
        let kind = TaskKind::from_persisted(
            None,
            Some("periodic_review"),
            Some("issue:42"),
            Some("issue #42"),
        )
        .expect("legacy task kind should decode");

        assert_eq!(kind, TaskKind::Issue);
    }

    #[test]
    fn legacy_pr_markers_override_planner_source() {
        let kind = TaskKind::from_persisted(
            Some("legacy"),
            Some("sprint_planner"),
            Some("pr:7"),
            Some("PR #7"),
        )
        .expect("legacy task kind should decode");

        assert_eq!(kind, TaskKind::Pr);
    }

    #[test]
    fn legacy_system_sources_still_decode_when_no_issue_or_pr_markers_exist() {
        let review = TaskKind::from_persisted(
            Some("legacy"),
            Some("periodic_review"),
            Some("legacy:review"),
            Some("periodic review"),
        )
        .expect("legacy review task kind should decode");
        let planner = TaskKind::from_persisted(
            Some("legacy"),
            Some("sprint_planner"),
            Some("legacy:planner"),
            Some("sprint planner"),
        )
        .expect("legacy planner task kind should decode");

        assert_eq!(review, TaskKind::Review);
        assert_eq!(planner, TaskKind::Planner);
    }
}
