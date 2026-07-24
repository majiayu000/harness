use super::IssueWorkflowStore;
use crate::issue_lifecycle::{
    IssueLifecycleEvent, IssueLifecycleEventKind, IssueMergeMethod, IssueMergePolicy,
    IssueWorkflowInstance,
};

impl IssueWorkflowStore {
    pub async fn record_issue_dependencies_detected(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        detail: Option<&str>,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(IssueLifecycleEventKind::DependenciesDetected);
            if let Some(detail) = detail {
                event = event.with_detail(detail.to_string());
            }
            Ok(workflow.apply_event(event)?)
        })
        .await
    }

    pub async fn record_issue_remote_fact(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        issue_url: Option<&str>,
        labels_snapshot: &[String],
        fact_hash: &str,
    ) -> anyhow::Result<IssueWorkflowInstance> {
        self.update_issue(project_id, repo, issue_number, |workflow| {
            if let Some(issue_url) = issue_url {
                workflow.set_issue_url(Some(issue_url.to_string()));
            }
            workflow.labels_snapshot = labels_snapshot.to_vec();
            workflow.set_remote_fact_hash(Some(fact_hash.to_string()));
            Ok(())
        })
        .await
    }

    pub async fn record_pr_remote_fact(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
        pr_head_sha: Option<&str>,
        fact_hash: &str,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            if let Some(pr_head_sha) = pr_head_sha {
                workflow.set_pr_head_sha(Some(pr_head_sha.to_string()));
            }
            workflow.set_remote_fact_hash(Some(fact_hash.to_string()));
            Ok(())
        })
        .await
    }

    pub async fn record_merge_policy_for_issue(
        &self,
        project_id: &str,
        repo: Option<&str>,
        issue_number: u64,
        merge_policy: Option<IssueMergePolicy>,
        merge_method: Option<IssueMergeMethod>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_existing_issue(project_id, repo, issue_number, |workflow| {
            workflow.set_merge_policy(merge_policy, merge_method);
            Ok(())
        })
        .await
    }

    pub async fn record_merge_started(
        &self,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
        task_id: &str,
        pr_head_sha: &str,
        fact_hash: Option<&str>,
    ) -> anyhow::Result<Option<IssueWorkflowInstance>> {
        self.update_by_pr(project_id, repo, pr_number, |workflow| {
            let mut event = IssueLifecycleEvent::new(IssueLifecycleEventKind::MergeStarted)
                .with_task_id(task_id.to_string())
                .with_pr_head_sha(pr_head_sha.to_string());
            if let Some(fact_hash) = fact_hash {
                event = event.with_remote_fact_hash(fact_hash.to_string());
            }
            Ok(workflow.apply_event(event)?)
        })
        .await
    }
}

#[cfg(test)]
mod lifecycle_transition_tests {
    use crate::issue_lifecycle::{
        IssueLifecycleEvent, IssueLifecycleEventKind, IssueLifecycleState,
        IssueLifecycleTransitionErrorReason, IssueWorkflowInstance, ReviewFallbackSnapshot,
        ReviewFallbackTier, ReviewFallbackTrigger,
    };
    use chrono::{DateTime, Duration, Utc};
    use std::collections::BTreeSet;

    #[test]
    fn issue_lifecycle_transition_matrix() {
        let states: Vec<IssueLifecycleState> = serde_json::from_str(
            r#"["discovered","awaiting_dependencies","scheduled","implementing","pr_open","awaiting_feedback","feedback_claimed","addressing_feedback","ready_to_merge","merging","done","blocked","failed","cancelled"]"#,
        )
        .unwrap();
        let events: Vec<IssueLifecycleEventKind> = serde_json::from_str(
            r#"["dependencies_detected","issue_scheduled","implement_started","plan_issue_detected","pr_detected","feedback_task_scheduled","feedback_sweep_completed","feedback_found","no_feedback_found","mergeable","merge_started","human_merge_approved","workflow_blocked","workflow_failed","workflow_cancelled","workflow_done"]"#,
        )
        .unwrap();
        let mut pairs = 0;
        for state in states {
            for kind in events.iter().copied() {
                let mut workflow = workflow_in(state);
                if state == IssueLifecycleState::AddressingFeedback
                    && kind == IssueLifecycleEventKind::FeedbackFound
                {
                    workflow.active_task_id = Some("claim:1".into());
                }
                let result = workflow.apply_event(matching_event(kind));
                assert_eq!(
                    result.is_ok(),
                    pair_allowed(state, kind),
                    "{state:?}/{kind:?}"
                );
                if result.is_ok() {
                    assert_eq!(workflow.state, expected_target(state, kind));
                }
                pairs += 1;
            }
        }
        assert_eq!(pairs, 224);
    }

    #[test]
    fn illegal_issue_lifecycle_transition_preserves_complete_snapshot() {
        let mut workflow = workflow_in(IssueLifecycleState::Done);
        workflow.review_fallback = Some(fallback(Utc::now()));
        let before = workflow.clone();
        let error = workflow
            .apply_event(matching_event(IssueLifecycleEventKind::PrDetected).with_pr(2, "other"))
            .unwrap_err();
        assert_eq!(
            error.reason,
            IssueLifecycleTransitionErrorReason::BindingConflict
        );
        assert_eq!(workflow, before);
    }

    #[test]
    fn terminal_issue_lifecycle_states_cannot_reopen() {
        for state in [
            IssueLifecycleState::Done,
            IssueLifecycleState::Failed,
            IssueLifecycleState::Cancelled,
        ] {
            for kind in [
                IssueLifecycleEventKind::IssueScheduled,
                IssueLifecycleEventKind::ImplementStarted,
                IssueLifecycleEventKind::PrDetected,
                IssueLifecycleEventKind::FeedbackFound,
                IssueLifecycleEventKind::Mergeable,
                IssueLifecycleEventKind::MergeStarted,
            ] {
                let mut workflow = workflow_in(state);
                let before = workflow.clone();
                assert!(workflow.apply_event(matching_event(kind)).is_err());
                assert_eq!(workflow, before);
            }
        }
    }

    #[test]
    fn accepted_issue_lifecycle_events_mutate_only_declared_fields() {
        for state in all_states() {
            for kind in all_events() {
                if !pair_allowed(state, kind) {
                    continue;
                }
                let policy = metadata_policy(state, kind);
                let mut workflow = workflow_in(state);
                let event = prepare_metadata_fixture(&mut workflow, state, kind, policy);
                let before = serde_json::to_value(&workflow).unwrap();
                workflow
                    .apply_event(event.with_remote_fact_hash("new-hash"))
                    .unwrap();
                let after = serde_json::to_value(&workflow).unwrap();
                let changed = changed_fields(&before, &after);
                let allowed = allowed_mutated_fields(policy);
                assert!(
                    changed.is_subset(&allowed),
                    "{state:?}/{kind:?}/{policy:?} changed undeclared fields: {:?}",
                    changed.difference(&allowed).collect::<Vec<_>>()
                );
                let required = required_mutated_fields(state, policy);
                assert!(
                    required.is_subset(&changed),
                    "{state:?}/{kind:?}/{policy:?} missed required mutations: {:?}",
                    required.difference(&changed).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn repeated_issue_lifecycle_bindings_require_matching_identity() {
        let mut scheduled = workflow_in(IssueLifecycleState::Scheduled);
        scheduled
            .apply_event(matching_event(IssueLifecycleEventKind::IssueScheduled))
            .unwrap();
        assert_binding_conflict_preserves(
            &mut scheduled,
            matching_event(IssueLifecycleEventKind::IssueScheduled).with_task_id("other-task"),
        );

        let mut pr_open = workflow_in(IssueLifecycleState::PrOpen);
        pr_open
            .apply_event(matching_event(IssueLifecycleEventKind::PrDetected))
            .unwrap();
        assert_binding_conflict_preserves(
            &mut pr_open,
            matching_event(IssueLifecycleEventKind::PrDetected).with_pr(2, "other"),
        );

        let mut merging = workflow_in(IssueLifecycleState::Merging);
        merging
            .apply_event(matching_event(IssueLifecycleEventKind::MergeStarted))
            .unwrap();
        assert_binding_conflict_preserves(
            &mut merging,
            matching_event(IssueLifecycleEventKind::MergeStarted).with_task_id("other-task"),
        );
        assert_binding_conflict_preserves(
            &mut merging,
            matching_event(IssueLifecycleEventKind::MergeStarted).with_pr_head_sha("other-head"),
        );

        let mut feedback = workflow_in(IssueLifecycleState::AddressingFeedback);
        feedback.active_task_id = Some("feedback-task-a".into());
        assert_binding_conflict_preserves(
            &mut feedback,
            matching_event(IssueLifecycleEventKind::FeedbackTaskScheduled)
                .with_task_id("feedback-task-b"),
        );
    }

    #[test]
    fn feedback_claim_placeholder_transitions_remain_recoverable() {
        let mut workflow = workflow_in(IssueLifecycleState::AddressingFeedback);
        workflow.active_task_id = Some("claim:1".into());
        workflow
            .apply_event(matching_event(IssueLifecycleEventKind::FeedbackFound))
            .unwrap();
        workflow
            .apply_event(
                matching_event(IssueLifecycleEventKind::FeedbackTaskScheduled)
                    .with_task_id("real-task"),
            )
            .unwrap();
        assert_eq!(workflow.active_task_id.as_deref(), Some("real-task"));
    }

    #[test]
    fn blocked_issue_lifecycle_can_converge_to_terminal_state() {
        for (kind, expected) in [
            (
                IssueLifecycleEventKind::WorkflowDone,
                IssueLifecycleState::Done,
            ),
            (
                IssueLifecycleEventKind::WorkflowFailed,
                IssueLifecycleState::Failed,
            ),
            (
                IssueLifecycleEventKind::WorkflowCancelled,
                IssueLifecycleState::Cancelled,
            ),
        ] {
            let mut workflow = workflow_in(IssueLifecycleState::Blocked);
            workflow.apply_event(matching_event(kind)).unwrap();
            assert_eq!(workflow.state, expected);
        }
    }

    fn workflow_in(state: IssueLifecycleState) -> IssueWorkflowInstance {
        let mut workflow = IssueWorkflowInstance::new("/tmp/p", Some("owner/repo".into()), 1);
        workflow.state = state;
        workflow.active_task_id = Some("task-1".into());
        workflow.pr_number = Some(1);
        workflow.pr_url = Some("pr-url".into());
        workflow.pr_head_sha = Some("head-1".into());
        workflow.merge_attempted_head_sha = Some("head-1".into());
        workflow
    }

    fn matching_event(kind: IssueLifecycleEventKind) -> IssueLifecycleEvent {
        let mut event = IssueLifecycleEvent::new(kind).with_task_id("task-1");
        event.pr_number = Some(1);
        event.pr_url = Some("pr-url".into());
        event.pr_head_sha = Some("head-1".into());
        event
    }

    fn fallback(activated_at: DateTime<Utc>) -> ReviewFallbackSnapshot {
        ReviewFallbackSnapshot {
            tier: ReviewFallbackTier::C,
            trigger: ReviewFallbackTrigger::Silence,
            active_bot: Some("codex".into()),
            activated_at,
        }
    }

    fn assert_binding_conflict_preserves(
        workflow: &mut IssueWorkflowInstance,
        event: IssueLifecycleEvent,
    ) {
        let before = workflow.clone();
        let error = workflow.apply_event(event).unwrap_err();
        assert_eq!(
            error.reason,
            IssueLifecycleTransitionErrorReason::BindingConflict
        );
        assert_eq!(*workflow, before);
    }

    fn changed_fields(before: &serde_json::Value, after: &serde_json::Value) -> BTreeSet<String> {
        let before = before.as_object().unwrap();
        let after = after.as_object().unwrap();
        before
            .keys()
            .chain(after.keys())
            .filter(|key| before.get(*key) != after.get(*key))
            .cloned()
            .collect()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MetadataPolicy {
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

    fn metadata_policy(
        source: IssueLifecycleState,
        kind: IssueLifecycleEventKind,
    ) -> MetadataPolicy {
        use IssueLifecycleEventKind as E;
        use IssueLifecycleState as S;
        use MetadataPolicy as P;
        match (source, kind) {
            (S::Done, E::HumanMergeApproved)
            | (S::Failed, E::WorkflowFailed)
            | (S::Cancelled, E::WorkflowCancelled) => P::AuditOnly,
            (_, E::DependenciesDetected) => P::DependenciesDetected,
            (_, E::IssueScheduled) => P::IssueScheduled,
            (_, E::ImplementStarted) => P::ImplementStarted,
            (_, E::PlanIssueDetected) => P::PlanIssueDetected,
            (_, E::PrDetected) => P::PrDetected,
            (_, E::FeedbackSweepCompleted) => P::FeedbackSweepCompleted,
            (_, E::FeedbackFound) => P::FeedbackFound,
            (_, E::FeedbackTaskScheduled) => P::FeedbackTaskScheduled,
            (_, E::NoFeedbackFound) => P::NoFeedbackFound,
            (_, E::Mergeable) => P::Mergeable,
            (_, E::MergeStarted) => P::MergeStarted,
            (_, E::HumanMergeApproved) => P::HumanMergeApproved,
            (_, E::WorkflowBlocked) => P::WorkflowBlocked,
            (_, E::WorkflowFailed) => P::WorkflowFailed,
            (_, E::WorkflowCancelled) => P::WorkflowCancelled,
            (_, E::WorkflowDone) => P::WorkflowDone,
        }
    }

    fn prepare_metadata_fixture(
        workflow: &mut IssueWorkflowInstance,
        source: IssueLifecycleState,
        kind: IssueLifecycleEventKind,
        policy: MetadataPolicy,
    ) -> IssueLifecycleEvent {
        use IssueLifecycleState as S;
        use MetadataPolicy as P;
        workflow.updated_at = Utc::now() - Duration::hours(1);
        workflow.feedback_claimed_at = Some(Utc::now() - Duration::minutes(1));
        workflow.review_fallback = Some(fallback(Utc::now() - Duration::minutes(2)));
        workflow.labels_snapshot = vec!["preserved".into()];
        workflow.plan_concern = Some("old concern".into());
        workflow.last_remote_fact_hash = Some("old-hash".into());
        match policy {
            P::IssueScheduled | P::ImplementStarted
                if matches!(source, S::Discovered | S::AwaitingDependencies) =>
            {
                workflow.active_task_id = None;
            }
            P::PrDetected => {
                workflow.pr_number = None;
                workflow.pr_url = None;
                workflow.pr_head_sha = None;
                if source == S::Discovered {
                    workflow.active_task_id = None;
                }
            }
            P::FeedbackFound => {
                workflow.pr_number = None;
                workflow.pr_url = None;
                workflow.pr_head_sha = None;
                if source == S::AddressingFeedback {
                    workflow.active_task_id = Some("claim:metadata".into());
                }
            }
            P::FeedbackTaskScheduled => {
                workflow.pr_number = None;
                workflow.pr_url = None;
                workflow.pr_head_sha = None;
                if source == S::AddressingFeedback {
                    workflow.active_task_id = Some("claim:metadata".into());
                }
            }
            P::Mergeable => workflow.pr_head_sha = None,
            P::MergeStarted if source == S::ReadyToMerge => {
                workflow.active_task_id = None;
                workflow.pr_head_sha = None;
                workflow.merge_attempted_head_sha = None;
            }
            P::WorkflowDone => {
                workflow.pr_number = None;
                workflow.pr_url = None;
                workflow.pr_head_sha = None;
            }
            _ => {}
        }
        let event = matching_event(kind);
        match policy {
            P::PlanIssueDetected => event.with_detail("new concern"),
            P::FeedbackTaskScheduled => event.with_task_id("feedback-task"),
            _ => event,
        }
    }

    fn allowed_mutated_fields(policy: MetadataPolicy) -> BTreeSet<String> {
        use MetadataPolicy as P;
        let mut fields = BTreeSet::from(
            ["state", "last_event", "updated_at", "last_remote_fact_hash"].map(str::to_string),
        );
        let specific: &[&str] = match policy {
            P::DependenciesDetected => &["active_task_id", "review_fallback"],
            P::IssueScheduled | P::ImplementStarted => &["active_task_id", "review_fallback"],
            P::PlanIssueDetected => &["active_task_id", "plan_concern", "review_fallback"],
            P::PrDetected => &[
                "active_task_id",
                "pr_number",
                "pr_url",
                "pr_head_sha",
                "review_fallback",
            ],
            P::FeedbackFound => &[
                "active_task_id",
                "feedback_claimed_at",
                "pr_number",
                "pr_url",
                "pr_head_sha",
            ],
            P::FeedbackTaskScheduled => &[
                "active_task_id",
                "feedback_claimed_at",
                "review_fallback",
                "pr_number",
                "pr_url",
                "pr_head_sha",
            ],
            P::FeedbackSweepCompleted | P::NoFeedbackFound => {
                &["active_task_id", "feedback_claimed_at", "review_fallback"]
            }
            P::Mergeable => &["active_task_id", "feedback_claimed_at", "pr_head_sha"],
            P::MergeStarted => &[
                "active_task_id",
                "feedback_claimed_at",
                "pr_head_sha",
                "merge_attempted_head_sha",
            ],
            P::HumanMergeApproved => &["feedback_claimed_at"],
            P::WorkflowBlocked => &["active_task_id", "feedback_claimed_at"],
            P::WorkflowFailed | P::WorkflowCancelled => &["feedback_claimed_at"],
            P::WorkflowDone => &["feedback_claimed_at", "pr_number", "pr_url", "pr_head_sha"],
            P::AuditOnly => &[],
        };
        fields.extend(specific.iter().map(|field| (*field).to_string()));
        fields
    }

    fn required_mutated_fields(
        source: IssueLifecycleState,
        policy: MetadataPolicy,
    ) -> BTreeSet<String> {
        use IssueLifecycleState as S;
        use MetadataPolicy as P;
        let mut fields = BTreeSet::from(
            ["last_event", "updated_at", "last_remote_fact_hash"].map(str::to_string),
        );
        if expected_target_for_policy(source, policy) != source {
            fields.insert("state".into());
        }
        let specific: &[&str] = match policy {
            P::DependenciesDetected => &["active_task_id", "review_fallback"],
            P::IssueScheduled | P::ImplementStarted
                if matches!(source, S::Discovered | S::AwaitingDependencies) =>
            {
                &["active_task_id", "review_fallback"]
            }
            P::IssueScheduled | P::ImplementStarted => &["review_fallback"],
            P::PlanIssueDetected => &["plan_concern", "review_fallback"],
            P::PrDetected if source == S::PrOpen => &["review_fallback"],
            P::PrDetected if source == S::Discovered => &[
                "active_task_id",
                "pr_number",
                "pr_url",
                "pr_head_sha",
                "review_fallback",
            ],
            P::PrDetected => &["pr_number", "pr_url", "pr_head_sha", "review_fallback"],
            P::FeedbackFound => &[
                "active_task_id",
                "feedback_claimed_at",
                "pr_number",
                "pr_url",
                "pr_head_sha",
            ],
            P::FeedbackTaskScheduled => &[
                "active_task_id",
                "feedback_claimed_at",
                "review_fallback",
                "pr_number",
                "pr_url",
                "pr_head_sha",
            ],
            P::FeedbackSweepCompleted | P::NoFeedbackFound => {
                &["active_task_id", "feedback_claimed_at", "review_fallback"]
            }
            P::Mergeable => &["active_task_id", "feedback_claimed_at", "pr_head_sha"],
            P::MergeStarted if source == S::Merging => &["feedback_claimed_at"],
            P::MergeStarted => &[
                "active_task_id",
                "feedback_claimed_at",
                "pr_head_sha",
                "merge_attempted_head_sha",
            ],
            P::HumanMergeApproved => &["feedback_claimed_at"],
            P::WorkflowBlocked => &["active_task_id", "feedback_claimed_at"],
            P::WorkflowFailed | P::WorkflowCancelled => &["feedback_claimed_at"],
            P::WorkflowDone => &["feedback_claimed_at", "pr_number", "pr_url", "pr_head_sha"],
            P::AuditOnly => &[],
        };
        fields.extend(specific.iter().map(|field| (*field).to_string()));
        fields
    }

    fn expected_target_for_policy(
        source: IssueLifecycleState,
        policy: MetadataPolicy,
    ) -> IssueLifecycleState {
        use IssueLifecycleState as S;
        use MetadataPolicy as P;
        match policy {
            P::DependenciesDetected => S::AwaitingDependencies,
            P::IssueScheduled => S::Scheduled,
            P::ImplementStarted if source == S::AddressingFeedback => S::AddressingFeedback,
            P::ImplementStarted | P::PlanIssueDetected => S::Implementing,
            P::PrDetected => S::PrOpen,
            P::FeedbackTaskScheduled => S::AddressingFeedback,
            P::FeedbackSweepCompleted | P::NoFeedbackFound => S::AwaitingFeedback,
            P::FeedbackFound => S::FeedbackClaimed,
            P::Mergeable => S::ReadyToMerge,
            P::MergeStarted => S::Merging,
            P::HumanMergeApproved | P::WorkflowDone => S::Done,
            P::WorkflowBlocked => S::Blocked,
            P::WorkflowFailed => S::Failed,
            P::WorkflowCancelled => S::Cancelled,
            P::AuditOnly => source,
        }
    }

    fn all_states() -> [IssueLifecycleState; 14] {
        use IssueLifecycleState as S;
        [
            S::Discovered,
            S::AwaitingDependencies,
            S::Scheduled,
            S::Implementing,
            S::PrOpen,
            S::AwaitingFeedback,
            S::FeedbackClaimed,
            S::AddressingFeedback,
            S::ReadyToMerge,
            S::Merging,
            S::Done,
            S::Blocked,
            S::Failed,
            S::Cancelled,
        ]
    }

    fn all_events() -> [IssueLifecycleEventKind; 16] {
        use IssueLifecycleEventKind as E;
        [
            E::DependenciesDetected,
            E::IssueScheduled,
            E::ImplementStarted,
            E::PlanIssueDetected,
            E::PrDetected,
            E::FeedbackTaskScheduled,
            E::FeedbackSweepCompleted,
            E::FeedbackFound,
            E::NoFeedbackFound,
            E::Mergeable,
            E::MergeStarted,
            E::HumanMergeApproved,
            E::WorkflowBlocked,
            E::WorkflowFailed,
            E::WorkflowCancelled,
            E::WorkflowDone,
        ]
    }

    fn expected_target(
        source: IssueLifecycleState,
        kind: IssueLifecycleEventKind,
    ) -> IssueLifecycleState {
        use IssueLifecycleEventKind as E;
        use IssueLifecycleState as S;
        match kind {
            E::DependenciesDetected => S::AwaitingDependencies,
            E::IssueScheduled => S::Scheduled,
            E::ImplementStarted if source == S::AddressingFeedback => S::AddressingFeedback,
            E::ImplementStarted | E::PlanIssueDetected => S::Implementing,
            E::PrDetected => S::PrOpen,
            E::FeedbackTaskScheduled => S::AddressingFeedback,
            E::FeedbackSweepCompleted | E::NoFeedbackFound => S::AwaitingFeedback,
            E::FeedbackFound => S::FeedbackClaimed,
            E::Mergeable => S::ReadyToMerge,
            E::MergeStarted => S::Merging,
            E::HumanMergeApproved | E::WorkflowDone => S::Done,
            E::WorkflowBlocked => S::Blocked,
            E::WorkflowFailed => S::Failed,
            E::WorkflowCancelled => S::Cancelled,
        }
    }

    fn pair_allowed(state: IssueLifecycleState, kind: IssueLifecycleEventKind) -> bool {
        use IssueLifecycleEventKind as E;
        use IssueLifecycleState as S;
        match kind {
            E::DependenciesDetected => matches!(state, S::Discovered | S::AwaitingDependencies),
            E::IssueScheduled => {
                matches!(
                    state,
                    S::Discovered | S::AwaitingDependencies | S::Scheduled
                )
            }
            E::ImplementStarted => matches!(
                state,
                S::Discovered
                    | S::AwaitingDependencies
                    | S::Scheduled
                    | S::Implementing
                    | S::AddressingFeedback
            ),
            E::PlanIssueDetected => matches!(state, S::Scheduled | S::Implementing),
            E::PrDetected => matches!(
                state,
                S::Discovered | S::Scheduled | S::Implementing | S::AddressingFeedback | S::PrOpen
            ),
            E::FeedbackTaskScheduled => {
                matches!(
                    state,
                    S::PrOpen | S::FeedbackClaimed | S::AddressingFeedback
                )
            }
            E::FeedbackSweepCompleted => matches!(state, S::PrOpen | S::AwaitingFeedback),
            E::FeedbackFound => matches!(
                state,
                S::PrOpen | S::AwaitingFeedback | S::FeedbackClaimed | S::AddressingFeedback
            ),
            E::NoFeedbackFound => matches!(state, S::FeedbackClaimed | S::AwaitingFeedback),
            E::Mergeable => matches!(
                state,
                S::PrOpen | S::AwaitingFeedback | S::AddressingFeedback | S::ReadyToMerge
            ),
            E::MergeStarted => matches!(state, S::ReadyToMerge | S::Merging),
            E::HumanMergeApproved => matches!(state, S::ReadyToMerge | S::Done),
            E::WorkflowBlocked => !matches!(state, S::Done | S::Failed | S::Cancelled),
            E::WorkflowFailed => !matches!(state, S::Done | S::Cancelled) || state == S::Failed,
            E::WorkflowCancelled => !matches!(state, S::Done | S::Failed) || state == S::Cancelled,
            E::WorkflowDone => !matches!(state, S::Failed | S::Cancelled) || state == S::Done,
        }
    }
}
