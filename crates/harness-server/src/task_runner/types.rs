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
        match source {
            Some("periodic_review") => Self::Review,
            Some("sprint_planner") => Self::Planner,
            _ => {
                if external_id.is_some_and(|id| id.starts_with("issue:"))
                    || description.is_some_and(|desc| desc.starts_with("issue #"))
                {
                    Self::Issue
                } else if external_id.is_some_and(|id| id.starts_with("pr:"))
                    || description.is_some_and(|desc| desc.starts_with("PR #"))
                {
                    Self::Pr
                } else {
                    Self::Prompt
                }
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

const TERMINAL_TASK_STATUSES: &[&str] = &["done", "failed", "cancelled"];
const RESUMABLE_TASK_STATUSES: &[&str] = &[
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
            Self::Implementing
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
            TaskStatus::Implementing => "implementing",
            TaskStatus::ReviewGenerating => "review_generating",
            TaskStatus::ReviewWaiting => "review_waiting",
            TaskStatus::PlannerGenerating => "planner_generating",
            TaskStatus::PlannerWaiting => "planner_waiting",
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
