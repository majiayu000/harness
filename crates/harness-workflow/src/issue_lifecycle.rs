use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Re-exported to preserve the `harness_workflow::issue_lifecycle::IssueWorkflowStore` path
/// used by callers outside this crate.
pub use crate::issue_workflow_store::IssueWorkflowStore;

const ISSUE_WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const FEEDBACK_CLAIM_TASK_PREFIX: &str = "claim:";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueLifecycleState {
    Discovered,
    Scheduled,
    Implementing,
    PrOpen,
    AwaitingFeedback,
    FeedbackClaimed,
    AddressingFeedback,
    ReadyToMerge,
    Blocked,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueLifecycleEventKind {
    IssueScheduled,
    ImplementStarted,
    PlanIssueDetected,
    PrDetected,
    FeedbackTaskScheduled,
    FeedbackSweepCompleted,
    FeedbackFound,
    NoFeedbackFound,
    Mergeable,
    /// Human approved the merge after reviewing a `ReadyToMerge` workflow.
    HumanMergeApproved,
    WorkflowFailed,
    WorkflowCancelled,
    WorkflowDone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueLifecycleEvent {
    pub kind: IssueLifecycleEventKind,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

impl IssueLifecycleEvent {
    pub(crate) fn new(kind: IssueLifecycleEventKind) -> Self {
        Self {
            kind,
            at: Utc::now(),
            task_id: None,
            detail: None,
            pr_number: None,
            pr_url: None,
        }
    }

    pub(crate) fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn with_pr(mut self, pr_number: u64, pr_url: impl Into<String>) -> Self {
        self.pr_number = Some(pr_number);
        self.pr_url = Some(pr_url.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueWorkflowInstance {
    pub id: String,
    pub schema_version: u32,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub issue_number: u64,
    pub state: IssueLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub labels_snapshot: Vec<String>,
    #[serde(default)]
    pub force_execute: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_concern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_claimed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event: Option<IssueLifecycleEvent>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl IssueWorkflowInstance {
    pub fn new(project_id: impl Into<String>, repo: Option<String>, issue_number: u64) -> Self {
        let project_id = project_id.into();
        let id = workflow_id(&project_id, repo.as_deref(), issue_number);
        let now = Utc::now();
        Self {
            id,
            schema_version: ISSUE_WORKFLOW_SCHEMA_VERSION,
            project_id,
            repo,
            issue_number,
            state: IssueLifecycleState::Discovered,
            active_task_id: None,
            pr_number: None,
            pr_url: None,
            labels_snapshot: Vec::new(),
            force_execute: false,
            plan_concern: None,
            feedback_claimed_at: None,
            last_event: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn apply_event(&mut self, event: IssueLifecycleEvent) {
        match event.kind {
            IssueLifecycleEventKind::IssueScheduled => {
                self.state = IssueLifecycleState::Scheduled;
                self.active_task_id = event.task_id.clone();
            }
            IssueLifecycleEventKind::ImplementStarted => {
                self.state = IssueLifecycleState::Implementing;
                self.active_task_id = event.task_id.clone();
            }
            IssueLifecycleEventKind::PlanIssueDetected => {
                self.state = IssueLifecycleState::Implementing;
                self.active_task_id = event.task_id.clone();
                self.plan_concern = event.detail.clone();
            }
            IssueLifecycleEventKind::PrDetected => {
                self.state = IssueLifecycleState::PrOpen;
                self.active_task_id = event.task_id.clone();
                self.pr_number = event.pr_number;
                self.pr_url = event.pr_url.clone();
            }
            IssueLifecycleEventKind::FeedbackFound => {
                self.state = IssueLifecycleState::FeedbackClaimed;
                self.active_task_id = None;
                self.feedback_claimed_at = Some(event.at);
                if event.pr_number.is_some() {
                    self.pr_number = event.pr_number;
                }
                if event.pr_url.is_some() {
                    self.pr_url = event.pr_url.clone();
                }
            }
            IssueLifecycleEventKind::FeedbackTaskScheduled => {
                self.state = IssueLifecycleState::AddressingFeedback;
                self.active_task_id = event.task_id.clone();
                self.feedback_claimed_at = None;
                if event.pr_number.is_some() {
                    self.pr_number = event.pr_number;
                }
                if event.pr_url.is_some() {
                    self.pr_url = event.pr_url.clone();
                }
            }
            IssueLifecycleEventKind::FeedbackSweepCompleted
            | IssueLifecycleEventKind::NoFeedbackFound => {
                self.state = IssueLifecycleState::AwaitingFeedback;
                self.active_task_id = None;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::Mergeable => {
                self.state = IssueLifecycleState::ReadyToMerge;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::HumanMergeApproved => {
                if self.state != IssueLifecycleState::ReadyToMerge {
                    tracing::warn!(
                        workflow_id = %self.id,
                        state = ?self.state,
                        "HumanMergeApproved received in unexpected state, ignoring"
                    );
                    return;
                }
                self.state = IssueLifecycleState::Done;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::WorkflowFailed => {
                self.state = IssueLifecycleState::Failed;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::WorkflowCancelled => {
                self.state = IssueLifecycleState::Cancelled;
                self.feedback_claimed_at = None;
            }
            IssueLifecycleEventKind::WorkflowDone => {
                self.state = IssueLifecycleState::Done;
                self.feedback_claimed_at = None;
            }
        }
        self.last_event = Some(event);
        self.updated_at = Utc::now();
    }
}

pub fn is_feedback_claim_placeholder(task_id: &str) -> bool {
    task_id.starts_with(FEEDBACK_CLAIM_TASK_PREFIX)
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}

pub fn workflow_id(project_id: &str, repo: Option<&str>, issue_number: u64) -> String {
    format!(
        "{project_id}::repo:{}::issue:{issue_number}",
        repo_key(repo)
    )
}

pub fn legacy_schema_for_path(path: &Path) -> anyhow::Result<String> {
    let path_utf8 = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {:?}", path))?;
    let digest = Sha256::digest(path_utf8.as_bytes());
    let mut schema_bytes = [0u8; 8];
    schema_bytes.copy_from_slice(&digest[..8]);
    Ok(format!("h{:016x}", u64::from_le_bytes(schema_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_merge_approved_transitions_ready_to_done() {
        let mut wf = IssueWorkflowInstance::new("/tmp/p", Some("owner/repo".to_string()), 1);
        wf.apply_event(IssueLifecycleEvent::new(IssueLifecycleEventKind::Mergeable));
        assert_eq!(wf.state, IssueLifecycleState::ReadyToMerge);

        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ));
        assert_eq!(wf.state, IssueLifecycleState::Done);
        assert!(wf.feedback_claimed_at.is_none());
        assert_eq!(
            wf.last_event.as_ref().map(|e| &e.kind),
            Some(&IssueLifecycleEventKind::HumanMergeApproved)
        );
    }

    #[test]
    fn human_merge_approved_is_noop_from_non_ready_state() {
        let mut wf = IssueWorkflowInstance::new("/tmp/p", Some("owner/repo".to_string()), 2);
        wf.apply_event(
            IssueLifecycleEvent::new(IssueLifecycleEventKind::IssueScheduled).with_task_id("t1"),
        );
        assert_eq!(wf.state, IssueLifecycleState::Scheduled);

        let last_event_before = wf.last_event.as_ref().map(|e| e.kind.clone());
        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ));
        assert_eq!(wf.state, IssueLifecycleState::Scheduled, "state unchanged");
        assert_eq!(
            wf.last_event.as_ref().map(|e| e.kind.clone()),
            last_event_before,
            "last_event unchanged on early return"
        );
    }

    #[test]
    fn human_merge_approved_double_fire_is_safe() {
        let mut wf = IssueWorkflowInstance::new("/tmp/p", Some("owner/repo".to_string()), 3);
        wf.apply_event(IssueLifecycleEvent::new(IssueLifecycleEventKind::Mergeable));
        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ));
        assert_eq!(wf.state, IssueLifecycleState::Done);

        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ));
        assert_eq!(wf.state, IssueLifecycleState::Done, "second call is no-op");
    }

    #[test]
    fn config_human_gate_flag_defaults_false() {
        let policy = harness_core::config::workflow::IssueWorkflowPolicy::default();
        assert!(!policy.require_human_gate_before_merge);
    }
}
