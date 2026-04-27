use harness_core::types::TaskId as CoreTaskId;
use serde::{Deserialize, Serialize};

pub type TaskId = CoreTaskId;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Review bot silent / quota-exhausted after all tiers; parked for human merge decision.
    ReadyToMerge,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFailureKind {
    Task,
    WorkspaceLifecycle,
}

const TERMINAL_TASK_STATUSES: &[&str] = &["done", "failed", "cancelled"];
const RESUMABLE_TASK_STATUSES: &[&str] = &[
    "triaging",
    "planning",
    "implementing",
    "review_generating",
    "review_waiting",
    "planner_generating",
    "planner_waiting",
    "agent_review",
    "waiting",
    "reviewing",
];

impl TaskStatus {
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
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::AwaitingDeps => "awaiting_deps",
            TaskStatus::Triaging => "triaging",
            TaskStatus::Planning => "planning",
            TaskStatus::Implementing => "implementing",
            TaskStatus::ReviewGenerating => "review_generating",
            TaskStatus::ReviewWaiting => "review_waiting",
            TaskStatus::PlannerGenerating => "planner_generating",
            TaskStatus::PlannerWaiting => "planner_waiting",
            TaskStatus::AgentReview => "agent_review",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Reviewing => "reviewing",
            TaskStatus::ReadyToMerge => "ready_to_merge",
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
            "ready_to_merge" => Ok(TaskStatus::ReadyToMerge),
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
