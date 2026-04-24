use harness_core::types::TaskId as CoreTaskId;
use serde::{Deserialize, Serialize};

pub type TaskId = CoreTaskId;

/// Explicit recovery contract for a task.
///
/// Each variant defines what data the recovery path needs and what outcome it
/// produces. Persisted in the `recovery_strategy` column so that future changes
/// to `TaskKind::recovery_strategy()` do not retroactively affect existing rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStrategy {
    /// Resume to `Pending` when a valid `pr_url` is present (task or checkpoint).
    RecoverByPr,
    /// Resume to `Pending` when `external_id` is present.
    RecoverByIssue,
    /// Resume to `ReviewWaiting`/`PlannerWaiting` when `system_input` is present.
    RecoverByPromptSnapshot,
    /// Resume to `Pending` when any checkpoint (pr, plan, triage) is present.
    RecoverByDerivedMetadata,
    /// Always transition to `Failed`; the task cannot be safely re-queued.
    #[default]
    NonRecoverable,
}

impl RecoveryStrategy {
    pub fn from_persisted(s: &str) -> Self {
        match s {
            "recover_by_pr" => Self::RecoverByPr,
            "recover_by_issue" => Self::RecoverByIssue,
            "recover_by_prompt_snapshot" => Self::RecoverByPromptSnapshot,
            "recover_by_derived_metadata" => Self::RecoverByDerivedMetadata,
            _ => Self::NonRecoverable,
        }
    }
}

impl AsRef<str> for RecoveryStrategy {
    fn as_ref(&self) -> &str {
        match self {
            Self::RecoverByPr => "recover_by_pr",
            Self::RecoverByIssue => "recover_by_issue",
            Self::RecoverByPromptSnapshot => "recover_by_prompt_snapshot",
            Self::RecoverByDerivedMetadata => "recover_by_derived_metadata",
            Self::NonRecoverable => "non_recoverable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Issue,
    Pr,
    #[default]
    Prompt,
    Review,
    Planner,
    /// A concrete implementation task spawned from a sprint-planner run.
    /// Unlike `Planner` (which generates the plan), `PlannerTask` executes
    /// a single item from that plan and is backed by a GitHub issue.
    PlannerTask,
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
            Some("planner_task") => Ok(Self::PlannerTask),
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
            Self::Issue | Self::Pr | Self::Prompt | Self::PlannerTask => TaskPhase::Implement,
        }
    }

    pub fn execution_status(&self) -> TaskStatus {
        match self {
            Self::Review => TaskStatus::ReviewGenerating,
            Self::Planner => TaskStatus::PlannerGenerating,
            Self::Issue | Self::Pr | Self::Prompt | Self::PlannerTask => TaskStatus::Implementing,
        }
    }

    /// Map each task kind to its explicit recovery contract.
    ///
    /// This replaces the old `recovery_status()` heuristic. The returned
    /// strategy is persisted in `tasks.recovery_strategy` so existing rows
    /// keep their original contract even if this mapping changes later.
    pub fn recovery_strategy(&self) -> RecoveryStrategy {
        match self {
            Self::Issue => RecoveryStrategy::RecoverByIssue,
            Self::Pr => RecoveryStrategy::RecoverByPr,
            Self::Prompt => RecoveryStrategy::RecoverByDerivedMetadata,
            Self::Review => RecoveryStrategy::RecoverByPromptSnapshot,
            Self::Planner => RecoveryStrategy::RecoverByPromptSnapshot,
            Self::PlannerTask => RecoveryStrategy::RecoverByIssue,
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
            Self::PlannerTask => "planner_task",
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

#[cfg(test)]
mod tests {
    use super::{RecoveryStrategy, TaskKind};

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

    #[test]
    fn task_kind_recovery_strategy_mappings() {
        assert_eq!(
            TaskKind::Issue.recovery_strategy(),
            RecoveryStrategy::RecoverByIssue
        );
        assert_eq!(
            TaskKind::Pr.recovery_strategy(),
            RecoveryStrategy::RecoverByPr
        );
        assert_eq!(
            TaskKind::Prompt.recovery_strategy(),
            RecoveryStrategy::RecoverByDerivedMetadata
        );
        assert_eq!(
            TaskKind::Review.recovery_strategy(),
            RecoveryStrategy::RecoverByPromptSnapshot
        );
        assert_eq!(
            TaskKind::Planner.recovery_strategy(),
            RecoveryStrategy::RecoverByPromptSnapshot
        );
        assert_eq!(
            TaskKind::PlannerTask.recovery_strategy(),
            RecoveryStrategy::RecoverByIssue
        );
    }

    #[test]
    fn recovery_strategy_as_ref_roundtrips() {
        let cases = [
            (RecoveryStrategy::RecoverByPr, "recover_by_pr"),
            (RecoveryStrategy::RecoverByIssue, "recover_by_issue"),
            (
                RecoveryStrategy::RecoverByPromptSnapshot,
                "recover_by_prompt_snapshot",
            ),
            (
                RecoveryStrategy::RecoverByDerivedMetadata,
                "recover_by_derived_metadata",
            ),
            (RecoveryStrategy::NonRecoverable, "non_recoverable"),
        ];
        for (variant, s) in cases {
            assert_eq!(variant.as_ref(), s);
            assert_eq!(RecoveryStrategy::from_persisted(s), variant);
        }
    }

    #[test]
    fn recovery_strategy_unknown_string_defaults_to_non_recoverable() {
        assert_eq!(
            RecoveryStrategy::from_persisted("bogus_value"),
            RecoveryStrategy::NonRecoverable
        );
    }

    #[test]
    fn recovery_strategy_serde_roundtrip() {
        let s = RecoveryStrategy::RecoverByPromptSnapshot;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""recover_by_prompt_snapshot""#);
        let back: RecoveryStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
