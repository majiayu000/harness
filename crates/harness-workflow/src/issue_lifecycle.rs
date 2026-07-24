use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Re-exported to preserve the `harness_workflow::issue_lifecycle::IssueWorkflowStore` path
/// used by callers outside this crate.
pub use crate::issue_workflow_store::{IssueMergeApprovalOutcome, IssueWorkflowStore};

const ISSUE_WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const FEEDBACK_CLAIM_TASK_PREFIX: &str = "claim:";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueLifecycleState {
    Discovered,
    AwaitingDependencies,
    Scheduled,
    Implementing,
    PrOpen,
    AwaitingFeedback,
    FeedbackClaimed,
    AddressingFeedback,
    ReadyToMerge,
    Merging,
    Done,
    Blocked,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueLifecycleEventKind {
    DependenciesDetected,
    IssueScheduled,
    ImplementStarted,
    PlanIssueDetected,
    PrDetected,
    FeedbackTaskScheduled,
    FeedbackSweepCompleted,
    FeedbackFound,
    NoFeedbackFound,
    Mergeable,
    MergeStarted,
    /// Human approved the merge after reviewing a `ReadyToMerge` workflow.
    HumanMergeApproved,
    WorkflowBlocked,
    WorkflowFailed,
    WorkflowCancelled,
    WorkflowDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLifecycleTransitionErrorReason {
    TransitionNotAllowed,
    BindingConflict,
}

impl std::fmt::Display for IssueLifecycleTransitionErrorReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransitionNotAllowed => formatter.write_str("transition_not_allowed"),
            Self::BindingConflict => formatter.write_str("binding_conflict"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "issue lifecycle transition rejected for {workflow_id}: state {source_state:?}, event {event_kind:?}, reason {reason}"
)]
pub struct IssueLifecycleTransitionError {
    pub workflow_id: String,
    pub source_state: IssueLifecycleState,
    pub event_kind: IssueLifecycleEventKind,
    pub reason: IssueLifecycleTransitionErrorReason,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueMergePolicy {
    Manual,
    Auto,
    Disabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueMergeMethod {
    Squash,
    Merge,
    Rebase,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFallbackSnapshot {
    pub tier: ReviewFallbackTier,
    pub trigger: ReviewFallbackTrigger,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_bot: Option<String>,
    pub activated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReviewFallbackTier {
    #[serde(rename = "a")]
    A,
    #[serde(rename = "b")]
    B,
    #[serde(rename = "c")]
    C,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFallbackTrigger {
    GeminiQuota,
    CodexQuota,
    AllBotsQuota,
    Silence,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_fact_hash: Option<String>,
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
            pr_head_sha: None,
            remote_fact_hash: None,
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

    pub(crate) fn with_pr_head_sha(mut self, pr_head_sha: impl Into<String>) -> Self {
        self.pr_head_sha = Some(pr_head_sha.into());
        self
    }

    pub(crate) fn with_remote_fact_hash(mut self, fact_hash: impl Into<String>) -> Self {
        self.remote_fact_hash = Some(fact_hash.into());
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_url: Option<String>,
    pub state: IssueLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_remote_fact_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_policy: Option<IssueMergePolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<IssueMergeMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_attempted_head_sha: Option<String>,
    #[serde(default)]
    pub labels_snapshot: Vec<String>,
    #[serde(default)]
    pub force_execute: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_concern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_claimed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_fallback: Option<ReviewFallbackSnapshot>,
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
            issue_url: None,
            state: IssueLifecycleState::Discovered,
            active_task_id: None,
            pr_number: None,
            pr_url: None,
            pr_head_sha: None,
            last_remote_fact_hash: None,
            merge_policy: None,
            merge_method: None,
            merge_attempted_head_sha: None,
            labels_snapshot: Vec::new(),
            force_execute: false,
            plan_concern: None,
            feedback_claimed_at: None,
            review_fallback: None,
            last_event: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn apply_event(
        &mut self,
        event: IssueLifecycleEvent,
    ) -> Result<(), IssueLifecycleTransitionError> {
        let decision = self.transition_decision(&event)?;
        self.apply_accepted_event(event, decision);
        Ok(())
    }

    pub(crate) fn apply_event_with_review_fallback(
        &mut self,
        event: IssueLifecycleEvent,
        fallback: ReviewFallbackSnapshot,
    ) -> Result<(), IssueLifecycleTransitionError> {
        let decision = self.transition_decision(&event)?;
        if event.kind != IssueLifecycleEventKind::Mergeable
            || self
                .review_fallback
                .as_ref()
                .is_some_and(|stored| !stored.has_same_logical_identity(&fallback))
        {
            return Err(self.transition_error(
                event.kind,
                IssueLifecycleTransitionErrorReason::BindingConflict,
            ));
        }
        self.apply_accepted_event(event, decision);
        if self.review_fallback.is_none() {
            self.review_fallback = Some(fallback);
        }
        Ok(())
    }

    fn transition_decision(
        &self,
        event: &IssueLifecycleEvent,
    ) -> Result<TransitionDecision, IssueLifecycleTransitionError> {
        use IssueLifecycleEventKind as Event;
        use IssueLifecycleState as State;
        use TransitionMetadataEffect as Effect;

        let source = self.state;
        let decision = match (source, event.kind) {
            (State::Discovered | State::AwaitingDependencies, Event::DependenciesDetected) => {
                TransitionDecision::new(State::AwaitingDependencies, Effect::DependenciesDetected)
            }
            (
                State::Discovered | State::AwaitingDependencies | State::Scheduled,
                Event::IssueScheduled,
            ) if source != State::Scheduled || compatible(&self.active_task_id, &event.task_id) => {
                TransitionDecision::new(State::Scheduled, Effect::IssueScheduled)
            }
            (
                State::Discovered
                | State::AwaitingDependencies
                | State::Scheduled
                | State::Implementing,
                Event::ImplementStarted,
            ) if !matches!(source, State::Scheduled | State::Implementing)
                || compatible(&self.active_task_id, &event.task_id) =>
            {
                TransitionDecision::new(State::Implementing, Effect::ImplementStarted)
            }
            (State::AddressingFeedback, Event::ImplementStarted)
                if compatible(&self.active_task_id, &event.task_id) =>
            {
                TransitionDecision::new(State::AddressingFeedback, Effect::ImplementStarted)
            }
            (State::Scheduled | State::Implementing, Event::PlanIssueDetected)
                if compatible(&self.active_task_id, &event.task_id) =>
            {
                TransitionDecision::new(State::Implementing, Effect::PlanIssueDetected)
            }
            (
                State::Discovered
                | State::Scheduled
                | State::Implementing
                | State::AddressingFeedback
                | State::PrOpen,
                Event::PrDetected,
            ) if (!matches!(source, State::PrOpen)
                || compatible(&self.active_task_id, &event.task_id))
                && self.pr_bindings_compatible(event) =>
            {
                TransitionDecision::new(State::PrOpen, Effect::PrDetected)
            }
            (State::PrOpen | State::AwaitingFeedback, Event::FeedbackSweepCompleted) => {
                TransitionDecision::new(State::AwaitingFeedback, Effect::FeedbackSweepCompleted)
            }
            (
                State::PrOpen | State::AwaitingFeedback | State::FeedbackClaimed,
                Event::FeedbackFound,
            ) if self.pr_bindings_compatible(event) => {
                TransitionDecision::new(State::FeedbackClaimed, Effect::FeedbackFound)
            }
            (State::AddressingFeedback, Event::FeedbackFound)
                if self
                    .active_task_id
                    .as_deref()
                    .is_some_and(is_feedback_claim_placeholder)
                    && self.pr_bindings_compatible(event) =>
            {
                TransitionDecision::new(State::FeedbackClaimed, Effect::FeedbackFound)
            }
            (State::PrOpen | State::FeedbackClaimed, Event::FeedbackTaskScheduled)
                if self.pr_bindings_compatible(event) =>
            {
                TransitionDecision::new(State::AddressingFeedback, Effect::FeedbackTaskScheduled)
            }
            (State::AddressingFeedback, Event::FeedbackTaskScheduled)
                if self.feedback_task_compatible(event) && self.pr_bindings_compatible(event) =>
            {
                TransitionDecision::new(State::AddressingFeedback, Effect::FeedbackTaskScheduled)
            }
            (State::FeedbackClaimed | State::AwaitingFeedback, Event::NoFeedbackFound) => {
                TransitionDecision::new(State::AwaitingFeedback, Effect::NoFeedbackFound)
            }
            (
                State::PrOpen
                | State::AwaitingFeedback
                | State::AddressingFeedback
                | State::ReadyToMerge,
                Event::Mergeable,
            ) if compatible(&self.pr_head_sha, &event.pr_head_sha) => {
                TransitionDecision::new(State::ReadyToMerge, Effect::Mergeable)
            }
            (State::ReadyToMerge | State::Merging, Event::MergeStarted)
                if compatible(&self.active_task_id, &event.task_id)
                    && compatible(&self.pr_head_sha, &event.pr_head_sha)
                    && compatible(&self.merge_attempted_head_sha, &event.pr_head_sha) =>
            {
                TransitionDecision::new(State::Merging, Effect::MergeStarted)
            }
            (State::ReadyToMerge, Event::HumanMergeApproved) => {
                TransitionDecision::new(State::Done, Effect::HumanMergeApproved)
            }
            (State::Done, Event::HumanMergeApproved) => {
                TransitionDecision::new(State::Done, Effect::AuditOnly)
            }
            (_, Event::WorkflowBlocked) if source.is_nonterminal() => {
                TransitionDecision::new(State::Blocked, Effect::WorkflowBlocked)
            }
            (_, Event::WorkflowFailed) if source.is_nonterminal() => {
                TransitionDecision::new(State::Failed, Effect::WorkflowFailed)
            }
            (State::Failed, Event::WorkflowFailed) => {
                TransitionDecision::new(State::Failed, Effect::AuditOnly)
            }
            (_, Event::WorkflowCancelled) if source.is_nonterminal() => {
                TransitionDecision::new(State::Cancelled, Effect::WorkflowCancelled)
            }
            (State::Cancelled, Event::WorkflowCancelled) => {
                TransitionDecision::new(State::Cancelled, Effect::AuditOnly)
            }
            (_, Event::WorkflowDone)
                if (source.is_nonterminal() || source == State::Done)
                    && self.pr_bindings_compatible(event) =>
            {
                TransitionDecision::new(State::Done, Effect::WorkflowDone)
            }
            _ => {
                let reason = if self.has_binding_conflict(event) {
                    IssueLifecycleTransitionErrorReason::BindingConflict
                } else {
                    IssueLifecycleTransitionErrorReason::TransitionNotAllowed
                };
                return Err(self.transition_error(event.kind, reason));
            }
        };
        Ok(decision)
    }

    fn apply_accepted_event(&mut self, event: IssueLifecycleEvent, decision: TransitionDecision) {
        use TransitionMetadataEffect as Effect;
        self.state = decision.target_state;
        match decision.metadata_effect {
            Effect::DependenciesDetected => {
                self.active_task_id = None;
                self.review_fallback = None;
            }
            Effect::IssueScheduled | Effect::ImplementStarted => {
                fill(&mut self.active_task_id, &event.task_id);
                self.review_fallback = None;
            }
            Effect::PlanIssueDetected => {
                fill(&mut self.active_task_id, &event.task_id);
                self.plan_concern = event.detail.clone();
                self.review_fallback = None;
            }
            Effect::PrDetected => {
                fill(&mut self.active_task_id, &event.task_id);
                self.fill_pr_bindings(&event);
                self.review_fallback = None;
            }
            Effect::FeedbackFound => {
                self.active_task_id = None;
                self.feedback_claimed_at = Some(event.at);
                self.fill_pr_bindings(&event);
            }
            Effect::FeedbackTaskScheduled => {
                fill(&mut self.active_task_id, &event.task_id);
                self.feedback_claimed_at = None;
                self.review_fallback = None;
                self.fill_pr_bindings(&event);
            }
            Effect::FeedbackSweepCompleted | Effect::NoFeedbackFound => {
                self.active_task_id = None;
                self.feedback_claimed_at = None;
                self.review_fallback = None;
            }
            Effect::Mergeable => {
                self.active_task_id = None;
                self.feedback_claimed_at = None;
                fill(&mut self.pr_head_sha, &event.pr_head_sha);
            }
            Effect::MergeStarted => {
                fill(&mut self.active_task_id, &event.task_id);
                self.feedback_claimed_at = None;
                fill(&mut self.pr_head_sha, &event.pr_head_sha);
                fill(&mut self.merge_attempted_head_sha, &event.pr_head_sha);
            }
            Effect::HumanMergeApproved => self.feedback_claimed_at = None,
            Effect::WorkflowBlocked => {
                self.active_task_id = None;
                self.feedback_claimed_at = None;
            }
            Effect::WorkflowFailed | Effect::WorkflowCancelled => {
                self.feedback_claimed_at = None;
            }
            Effect::WorkflowDone => {
                self.feedback_claimed_at = None;
                self.fill_pr_bindings(&event);
            }
            Effect::AuditOnly => {}
        }
        if event.remote_fact_hash.is_some() {
            self.last_remote_fact_hash = event.remote_fact_hash.clone();
        }
        self.last_event = Some(event);
        self.updated_at = Utc::now();
    }

    fn transition_error(
        &self,
        event_kind: IssueLifecycleEventKind,
        reason: IssueLifecycleTransitionErrorReason,
    ) -> IssueLifecycleTransitionError {
        IssueLifecycleTransitionError {
            workflow_id: self.id.clone(),
            source_state: self.state,
            event_kind,
            reason,
        }
    }

    fn pr_bindings_compatible(&self, event: &IssueLifecycleEvent) -> bool {
        compatible(&self.pr_number, &event.pr_number)
            && compatible(&self.pr_url, &event.pr_url)
            && compatible(&self.pr_head_sha, &event.pr_head_sha)
    }

    fn feedback_task_compatible(&self, event: &IssueLifecycleEvent) -> bool {
        self.active_task_id
            .as_deref()
            .is_some_and(is_feedback_claim_placeholder)
            || compatible(&self.active_task_id, &event.task_id)
    }

    fn has_binding_conflict(&self, event: &IssueLifecycleEvent) -> bool {
        !self.pr_bindings_compatible(event)
            || matches!(
                event.kind,
                IssueLifecycleEventKind::IssueScheduled
                    | IssueLifecycleEventKind::ImplementStarted
                    | IssueLifecycleEventKind::PlanIssueDetected
                    | IssueLifecycleEventKind::PrDetected
                    | IssueLifecycleEventKind::FeedbackTaskScheduled
                    | IssueLifecycleEventKind::MergeStarted
            ) && !compatible(&self.active_task_id, &event.task_id)
            || event.kind == IssueLifecycleEventKind::MergeStarted
                && !compatible(&self.merge_attempted_head_sha, &event.pr_head_sha)
    }

    fn fill_pr_bindings(&mut self, event: &IssueLifecycleEvent) {
        fill(&mut self.pr_number, &event.pr_number);
        fill(&mut self.pr_url, &event.pr_url);
        fill(&mut self.pr_head_sha, &event.pr_head_sha);
    }

    pub fn set_review_fallback(&mut self, fallback: Option<ReviewFallbackSnapshot>) {
        self.review_fallback = fallback;
        self.updated_at = Utc::now();
    }

    pub fn set_issue_url(&mut self, issue_url: Option<String>) {
        self.issue_url = issue_url;
        self.updated_at = Utc::now();
    }

    pub fn set_remote_fact_hash(&mut self, fact_hash: Option<String>) {
        self.last_remote_fact_hash = fact_hash;
        self.updated_at = Utc::now();
    }

    pub fn set_pr_head_sha(&mut self, pr_head_sha: Option<String>) {
        self.pr_head_sha = pr_head_sha;
        self.updated_at = Utc::now();
    }

    pub fn set_merge_policy(
        &mut self,
        merge_policy: Option<IssueMergePolicy>,
        merge_method: Option<IssueMergeMethod>,
    ) {
        self.merge_policy = merge_policy;
        self.merge_method = merge_method;
        self.updated_at = Utc::now();
    }
}

impl IssueLifecycleState {
    fn is_nonterminal(self) -> bool {
        !matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

impl ReviewFallbackSnapshot {
    fn has_same_logical_identity(&self, other: &Self) -> bool {
        self.tier == other.tier
            && self.trigger == other.trigger
            && self.active_bot == other.active_bot
    }
}

#[derive(Debug, Clone, Copy)]
struct TransitionDecision {
    target_state: IssueLifecycleState,
    metadata_effect: TransitionMetadataEffect,
}

impl TransitionDecision {
    fn new(target_state: IssueLifecycleState, metadata_effect: TransitionMetadataEffect) -> Self {
        Self {
            target_state,
            metadata_effect,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TransitionMetadataEffect {
    DependenciesDetected,
    IssueScheduled,
    ImplementStarted,
    PlanIssueDetected,
    PrDetected,
    FeedbackSweepCompleted,
    FeedbackFound,
    FeedbackTaskScheduled,
    NoFeedbackFound,
    Mergeable,
    MergeStarted,
    HumanMergeApproved,
    WorkflowBlocked,
    WorkflowFailed,
    WorkflowCancelled,
    WorkflowDone,
    AuditOnly,
}

fn compatible<T: PartialEq>(stored: &Option<T>, incoming: &Option<T>) -> bool {
    match (stored, incoming) {
        (Some(stored), Some(incoming)) => stored == incoming,
        _ => true,
    }
}

fn fill<T: Clone>(stored: &mut Option<T>, incoming: &Option<T>) {
    if let Some(incoming) = incoming {
        *stored = Some(incoming.clone());
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
        wf.state = IssueLifecycleState::PrOpen;
        wf.apply_event(IssueLifecycleEvent::new(IssueLifecycleEventKind::Mergeable))
            .unwrap();
        assert_eq!(wf.state, IssueLifecycleState::ReadyToMerge);

        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ))
        .unwrap();
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
        )
        .unwrap();
        assert_eq!(wf.state, IssueLifecycleState::Scheduled);

        let before = wf.clone();
        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ))
        .unwrap_err();
        assert_eq!(wf, before);
    }

    #[test]
    fn human_merge_approved_double_fire_is_safe() {
        let mut wf = IssueWorkflowInstance::new("/tmp/p", Some("owner/repo".to_string()), 3);
        wf.state = IssueLifecycleState::PrOpen;
        wf.apply_event(IssueLifecycleEvent::new(IssueLifecycleEventKind::Mergeable))
            .unwrap();
        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ))
        .unwrap();
        assert_eq!(wf.state, IssueLifecycleState::Done);

        wf.apply_event(IssueLifecycleEvent::new(
            IssueLifecycleEventKind::HumanMergeApproved,
        ))
        .unwrap();
        assert_eq!(wf.state, IssueLifecycleState::Done);
    }

    #[test]
    fn mergeable_preserves_existing_review_fallback_snapshot() {
        let mut wf = IssueWorkflowInstance::new("/tmp/p", Some("owner/repo".to_string()), 4);
        wf.state = IssueLifecycleState::PrOpen;
        let activated_at = Utc::now();
        wf.set_review_fallback(Some(ReviewFallbackSnapshot {
            tier: ReviewFallbackTier::C,
            trigger: ReviewFallbackTrigger::Silence,
            active_bot: Some("codex".to_string()),
            activated_at,
        }));

        wf.apply_event(IssueLifecycleEvent::new(IssueLifecycleEventKind::Mergeable))
            .unwrap();

        assert_eq!(wf.state, IssueLifecycleState::ReadyToMerge);
        assert_eq!(
            wf.review_fallback,
            Some(ReviewFallbackSnapshot {
                tier: ReviewFallbackTier::C,
                trigger: ReviewFallbackTrigger::Silence,
                active_bot: Some("codex".to_string()),
                activated_at,
            })
        );
    }

    #[test]
    fn config_human_gate_flag_defaults_false() {
        let policy = harness_core::config::workflow::IssueWorkflowPolicy::default();
        assert!(!policy.require_human_gate_before_merge);
    }
}
