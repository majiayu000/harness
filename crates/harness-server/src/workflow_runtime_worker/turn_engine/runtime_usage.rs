use harness_core::agent::AgentBackend;
use harness_core::config::workflow::{RuntimeBudgetEnforcement, RuntimeBudgetPolicy};
use harness_core::run_id::RunId;
use harness_core::types::{TokenUsage, TurnId};
use harness_workflow::runtime::{
    cost_usd_from_micros, cost_usd_to_micros, RuntimeKind, RuntimeUsageMetrics, RuntimeUsageUpsert,
    RuntimeUsageUpsertOutcome, WorkflowRuntimeStore,
};
use serde_json::json;
use std::sync::Arc;

/// Mid-turn budget stop (GH-1770 spec §4.3): the streamed usage that was just
/// persisted put the workflow at or over its USD ceiling, so the in-flight
/// turn must be interrupted rather than allowed to keep spending.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnBudgetStop {
    pub(crate) workflow_id: String,
    pub(crate) spent_usd: f64,
    pub(crate) budget_usd: f64,
}

pub(crate) fn budget_stop_artifact(
    stop: &TurnBudgetStop,
) -> harness_workflow::runtime::ActivityArtifact {
    harness_workflow::runtime::ActivityArtifact::new(
        harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_BUDGET_STOP,
        serde_json::json!({
            "workflow_id": stop.workflow_id,
            "spent_usd": stop.spent_usd,
            "budget_usd": stop.budget_usd,
            "enforcement": "enforce",
        }),
    )
}

pub(crate) fn enforced_budget_cost_error(
    backend: &dyn AgentBackend,
    policy: &RuntimeBudgetPolicy,
) -> Option<String> {
    (!policy.unlimited
        && policy.enforcement == RuntimeBudgetEnforcement::Enforce
        && !backend.reports_usage_cost())
    .then(|| {
        format!(
            "agent backend `{}` does not report USD cost; refusing to launch under enforced USD budget policy",
            backend.name()
        )
    })
}

#[derive(Clone)]
pub(crate) struct RuntimeUsageContext {
    pub(crate) store: Arc<WorkflowRuntimeStore>,
    pub(crate) runtime_job_id: String,
    pub(crate) command_id: String,
    pub(crate) workflow_id: String,
    pub(crate) agent_run_id: Option<RunId>,
    pub(crate) runtime_kind: RuntimeKind,
    pub(crate) runtime_profile: String,
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) project: String,
    pub(crate) task_id: Option<String>,
    pub(crate) candidate_group_id: Option<String>,
    pub(crate) candidate_id: Option<String>,
    pub(crate) candidate_index: Option<u32>,
    pub(crate) candidate_count: Option<u32>,
    pub(crate) budget_policy: RuntimeBudgetPolicy,
}

impl std::fmt::Debug for RuntimeUsageContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeUsageContext")
            .field("runtime_job_id", &self.runtime_job_id)
            .field("command_id", &self.command_id)
            .field("workflow_id", &self.workflow_id)
            .field("runtime_kind", &self.runtime_kind)
            .field("runtime_profile", &self.runtime_profile)
            .field("agent", &self.agent)
            .field("model", &self.model)
            .field("project", &self.project)
            .field("task_id", &self.task_id)
            .field("candidate_group_id", &self.candidate_group_id)
            .field("candidate_id", &self.candidate_id)
            .field("candidate_index", &self.candidate_index)
            .field("candidate_count", &self.candidate_count)
            .finish_non_exhaustive()
    }
}

impl RuntimeUsageContext {
    fn usage_record(
        &self,
        turn_id: &TurnId,
        metrics: RuntimeUsageMetrics,
        cost_usd_micros: u64,
        cost_usd_observed: bool,
    ) -> RuntimeUsageUpsert {
        RuntimeUsageUpsert {
            runtime_job_id: self.runtime_job_id.clone(),
            command_id: self.command_id.clone(),
            workflow_id: self.workflow_id.clone(),
            turn_id: Some(turn_id.as_str().to_string()),
            agent_run_id: self.agent_run_id.clone(),
            runtime_kind: self.runtime_kind,
            runtime_profile: self.runtime_profile.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            project: self.project.clone(),
            task_id: self.task_id.clone(),
            candidate_group_id: self.candidate_group_id.clone(),
            candidate_id: self.candidate_id.clone(),
            candidate_index: self.candidate_index,
            candidate_count: self.candidate_count,
            metrics,
            cost_usd_micros,
            cost_usd_observed,
            reported_at: chrono::Utc::now(),
        }
    }

    pub(crate) async fn persist_agent_run_start(&self, turn_id: &TurnId) -> anyhow::Result<()> {
        self.store
            .upsert_runtime_agent_run(&self.usage_record(
                turn_id,
                RuntimeUsageMetrics::default(),
                0,
                false,
            ))
            .await
    }

    pub(crate) async fn persist_cost(&self, turn_id: &TurnId, cost_usd: f64) -> anyhow::Result<()> {
        // This event reports no token counts. The store preserves independent
        // token observations and an explicitly observed zero USD cost.
        self.store
            .upsert_runtime_agent_run(&self.usage_record(
                turn_id,
                RuntimeUsageMetrics::default(),
                cost_usd_to_micros(cost_usd)?,
                true,
            ))
            .await
    }

    pub(crate) async fn persist_token_usage(
        &self,
        turn_id: &TurnId,
        usage: &TokenUsage,
        cost_usd_observed: bool,
    ) -> anyhow::Result<()> {
        match self
            .store
            .upsert_runtime_usage(&self.usage_record(
                turn_id,
                RuntimeUsageMetrics::from_token_usage(usage),
                cost_usd_to_micros(usage.cost_usd)?,
                cost_usd_observed,
            ))
            .await?
        {
            RuntimeUsageUpsertOutcome::SkippedZeroUsage => {}
            RuntimeUsageUpsertOutcome::Persisted => {}
        }
        Ok(())
    }

    pub(crate) async fn budget_stop(&self) -> anyhow::Result<Option<TurnBudgetStop>> {
        if self.budget_policy.unlimited {
            return Ok(None);
        }
        let budget_usd = self.budget_policy.default_workflow_budget_usd;
        let budget_usd_micros = cost_usd_to_micros(budget_usd)?;
        let spent_usd_micros = self
            .store
            .runtime_usage_for_workflow(&self.workflow_id)
            .await?
            .map(|usage| usage.cost_usd_micros)
            .unwrap_or(0);
        if spent_usd_micros < budget_usd_micros {
            return Ok(None);
        }
        let spent_usd = cost_usd_from_micros(spent_usd_micros);
        match self.budget_policy.enforcement {
            RuntimeBudgetEnforcement::Shadow => {
                self.store
                    .append_event(
                        &self.workflow_id,
                        "BudgetShadowDecision",
                        "workflow_runtime_turn_watchdog",
                        json!({
                            "decision": "would_interrupt",
                            "spent_usd": spent_usd,
                            "budget_usd": budget_usd,
                            "runtime_job_id": self.runtime_job_id,
                            "command_id": self.command_id,
                        }),
                    )
                    .await?;
                Ok(None)
            }
            RuntimeBudgetEnforcement::Enforce => Ok(Some(TurnBudgetStop {
                workflow_id: self.workflow_id.clone(),
                spent_usd,
                budget_usd,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::agent::{AgentRequest, StreamItem};
    use harness_core::config::HarnessConfig;
    use harness_core::error::HarnessError;
    use harness_core::types::{AgentId, Item, TurnFailureKind, TurnStatus};
    use harness_protocol::notifications::{Notification, RpcNotification};
    use harness_workflow::runtime::{ActivityErrorKind, ActivityStatus, RuntimeJob};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{broadcast, mpsc};

    struct AccountingBackend {
        store: Arc<WorkflowRuntimeStore>,
        event: StreamItem,
        close_pool: bool,
        http_failure: Option<u16>,
        notifications: broadcast::Sender<RpcNotification>,
        active: AtomicBool,
        continued: AtomicBool,
        drains: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AgentBackend for AccountingBackend {
        fn name(&self) -> &str {
            "accounting-test"
        }

        fn reports_usage_cost(&self) -> bool {
            true
        }

        async fn execute_stream(
            &self,
            _request: AgentRequest,
            tx: mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            struct ActiveExecution<'a>(&'a AtomicBool);
            impl Drop for ActiveExecution<'_> {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            self.active.store(true, Ordering::SeqCst);
            let _active = ActiveExecution(&self.active);
            if self.close_pool {
                // Fail only after admission and the initial run mapping were persisted.
                self.store.pool().close().await;
            }
            if let Some(status) = self.http_failure {
                // Queue both items and return Err in one poll. The lifecycle must
                // observe the backend failure before processing the buffered usage.
                tx.try_send(self.event.clone()).unwrap();
                tx.try_send(StreamItem::ItemCompleted {
                    item: Item::AgentReasoning {
                        content: "buffered after accounting".into(),
                    },
                })
                .unwrap();
                return Err(HarnessError::AgentHttpResponse {
                    provider: "scripted".into(),
                    status,
                    message: "scripted backend failure".into(),
                });
            }
            let mut notifications = self.notifications.subscribe();
            tx.send(self.event.clone()).await.unwrap();
            tx.send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: "continued after accounting".into(),
                },
            })
            .await
            .unwrap();
            // Acknowledgement makes continuation deterministic, without timing sleeps.
            while let Ok(notification) = notifications.recv().await {
                if matches!(
                    notification.notification,
                    Notification::ItemCompleted { .. }
                ) {
                    self.continued.store(true, Ordering::SeqCst);
                    return Ok(());
                }
            }
            unreachable!("the lifecycle retains the notification sender")
        }

        async fn start_turn(
            &self,
            request: AgentRequest,
            tx: mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.execute_stream(request, tx).await
        }

        async fn terminate_and_drain(&self) -> harness_core::error::Result<()> {
            assert!(
                !self.active.load(Ordering::SeqCst),
                "drop execution before drain"
            );
            self.drains.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn accounting_case(
        cost_only: bool,
        fault: Option<&str>,
        force_code_agent: bool,
        policy: RuntimeBudgetPolicy,
        http_failure: Option<u16>,
    ) -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let database_url = crate::test_helpers::test_database_url()?;
        let store = Arc::new(
            WorkflowRuntimeStore::open_with_database_url(
                &root.path().join("runtime"),
                Some(&database_url),
            )
            .await?,
        );
        let context = RuntimeUsageContext {
            store: store.clone(),
            runtime_job_id: "accounting-job".into(),
            command_id: "accounting-command".into(),
            workflow_id: "accounting-workflow".into(),
            agent_run_id: None,
            runtime_kind: RuntimeKind::CodexExec,
            runtime_profile: "scripted".into(),
            agent: "accounting-test".into(),
            model: "scripted".into(),
            project: root.path().display().to_string(),
            task_id: None,
            candidate_group_id: None,
            candidate_id: None,
            candidate_index: None,
            candidate_count: None,
            budget_policy: policy.clone(),
        };
        store
            .upsert_instance(
                &harness_workflow::runtime::WorkflowInstance::new(
                    "github_issue_pr",
                    1,
                    "discovered",
                    harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1770"),
                )
                .with_id(&context.workflow_id),
            )
            .await?;
        if fault == Some("watchdog") {
            // Valid individual rows force PostgreSQL's aggregate BIGINT cast to fail.
            // Use the public writer; no shared tables or failure hooks are changed.
            for id in ["prior-turn-1", "prior-turn-2"] {
                context
                    .persist_token_usage(
                        &TurnId::from_str(id),
                        &TokenUsage {
                            input_tokens: i64::MAX as u64,
                            total_tokens: i64::MAX as u64,
                            ..Default::default()
                        },
                        true,
                    )
                    .await?;
            }
            assert!(context.budget_stop().await.is_err() || policy.unlimited);
        }
        let (notification_tx, _) = broadcast::channel(16);
        let backend = Arc::new(AccountingBackend {
            store: store.clone(),
            event: if cost_only {
                StreamItem::CostReported { cost_usd: 0.125 }
            } else {
                StreamItem::TokenUsage {
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                        cost_usd: 0.125,
                    },
                    cost_usd_observed: true,
                }
            },
            close_pool: fault == Some("persist"),
            http_failure,
            notifications: notification_tx.clone(),
            active: AtomicBool::new(false),
            continued: AtomicBool::new(false),
            drains: AtomicUsize::new(0),
        });
        let mut registry = AgentRegistry::new("accounting-test");
        registry.register("accounting-test", backend.clone());
        let mut config = HarnessConfig::default();
        config.server.project_root = root.path().to_path_buf();
        let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
        let thread_id = server
            .thread_manager
            .start_thread(root.path().to_path_buf());
        let turn_id = server.thread_manager.start_turn(
            &thread_id,
            "scripted accounting".into(),
            AgentId::from_str("accounting-test"),
        )?;
        run_turn_lifecycle_with_options(
            server.clone(),
            None,
            notification_tx,
            thread_id.clone(),
            turn_id.clone(),
            "scripted accounting".into(),
            "accounting-test".into(),
            TurnLifecycleOptions {
                selected_backend: Some(backend.clone()),
                force_code_agent,
                runtime_usage: Some(context),
                timeout_secs: Some(5),
                ..Default::default()
            },
        )
        .await;
        let should_stop = policy.enforcement == RuntimeBudgetEnforcement::Enforce
            && !policy.unlimited
            && (fault.is_some() || policy.default_workflow_budget_usd <= 0.125);
        let turn = server
            .thread_manager
            .get_turn(&thread_id, &turn_id)
            .unwrap();
        assert_eq!(
            turn.status,
            if should_stop {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            },
            "cost_only={cost_only}, fault={fault:?}, oneshot={force_code_agent}, policy={policy:?}"
        );
        assert_eq!(backend.continued.load(Ordering::SeqCst), !should_stop);
        assert!(!backend.active.load(Ordering::SeqCst));
        assert_eq!(
            backend.drains.load(Ordering::SeqCst),
            usize::from(should_stop && !force_code_agent)
        );
        if should_stop {
            let expected = match fault {
                Some("persist") => "failed to persist workflow runtime",
                Some("watchdog") => "failed to evaluate the mid-turn workflow budget ceiling",
                _ => "reaching its 0.10 USD budget",
            };
            assert!(turn.items.iter().any(
                |item| matches!(item, Item::Error { message, .. } if message.contains(expected))
            ));
            assert!(!turn
                .items
                .iter()
                .any(|item| matches!(item, Item::AgentReasoning { .. })));
        }
        if let Some(status) = http_failure {
            let (expected_failure, expected_activity) = match status {
                402 => (TurnFailureKind::Billing, ActivityErrorKind::Configuration),
                429 => (TurnFailureKind::Quota, ActivityErrorKind::Configuration),
                503 => (
                    TurnFailureKind::Upstream,
                    ActivityErrorKind::ExternalDependency,
                ),
                _ => unreachable!("unexpected test status"),
            };
            assert!(
                matches!(
                    turn.items.last(),
                    Some(Item::Error { message, failure_kind, .. })
                        if message == "agent execution failed: scripted backend failure"
                            && *failure_kind == Some(expected_failure)
                ),
                "backend failure must remain primary: {:?}",
                turn.items
            );
            let job = RuntimeJob::pending(
                "accounting-command",
                RuntimeKind::CodexExec,
                "scripted",
                json!({ "activity": "implement_issue" }),
            );
            let result = crate::workflow_runtime_worker::activity_result::activity_result_from_turn_with_workflow(
                &job,
                &turn.status,
                &turn.items,
                &thread_id,
                &turn_id,
                "accounting-test",
                root.path(),
                "test-digest",
                None,
            );
            assert_eq!(result.status, ActivityStatus::Failed);
            assert_eq!(result.error_kind, Some(expected_activity));
            assert_eq!(
                result.error.as_deref(),
                Some("agent execution failed: scripted backend failure")
            );
        }
        let inspection_store = if fault == Some("persist") {
            Arc::new(
                WorkflowRuntimeStore::open_with_database_url(
                    &root.path().join("runtime"),
                    Some(&database_url),
                )
                .await?,
            )
        } else {
            store
        };
        let records = inspection_store
            .runtime_usage_between(
                chrono::Utc::now() - chrono::Duration::minutes(1),
                chrono::Utc::now(),
            )
            .await?;
        let record = records
            .iter()
            .find(|row| row.turn_id.as_deref() == Some(turn_id.as_str()))
            .unwrap();
        let persisted = fault != Some("persist");
        assert_eq!(record.cost_usd_micros, if persisted { 125_000 } else { 0 });
        assert_eq!(record.cost_usd_observed, persisted);
        assert_eq!(
            record.metrics.input_tokens,
            if persisted && !cost_only { 10 } else { 0 }
        );
        Ok(())
    }

    async fn accounting_error_cases(cost_only: bool, fault: &str) -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        for policy in [
            RuntimeBudgetPolicy {
                enforcement: RuntimeBudgetEnforcement::Enforce,
                ..Default::default()
            },
            RuntimeBudgetPolicy::default(),
            RuntimeBudgetPolicy {
                enforcement: RuntimeBudgetEnforcement::Enforce,
                unlimited: true,
                ..Default::default()
            },
        ] {
            for oneshot in [true, false] {
                accounting_case(cost_only, Some(fault), oneshot, policy.clone(), None).await?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_stream_token_persistence_failure() -> anyhow::Result<()> {
        accounting_error_cases(false, "persist").await
    }

    #[tokio::test]
    async fn ordinary_stream_cost_persistence_failure() -> anyhow::Result<()> {
        accounting_error_cases(true, "persist").await
    }

    #[tokio::test]
    async fn ordinary_stream_token_watchdog_failure() -> anyhow::Result<()> {
        accounting_error_cases(false, "watchdog").await
    }

    #[tokio::test]
    async fn ordinary_stream_cost_watchdog_failure() -> anyhow::Result<()> {
        accounting_error_cases(true, "watchdog").await
    }

    #[tokio::test]
    async fn ordinary_stream_accounting_failure_preserves_backend_failure() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        for status in [402, 429, 503] {
            for cost_only in [true, false] {
                for fault in ["persist", "watchdog"] {
                    for oneshot in [true, false] {
                        accounting_case(
                            cost_only,
                            Some(fault),
                            oneshot,
                            RuntimeBudgetPolicy {
                                enforcement: RuntimeBudgetEnforcement::Enforce,
                                ..Default::default()
                            },
                            Some(status),
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_stream_successful_accounting_controls() -> anyhow::Result<()> {
        if !crate::test_helpers::db_tests_enabled().await {
            return Ok(());
        }
        for cost_only in [true, false] {
            for oneshot in [true, false] {
                for policy in [
                    RuntimeBudgetPolicy {
                        enforcement: RuntimeBudgetEnforcement::Enforce,
                        ..Default::default()
                    },
                    RuntimeBudgetPolicy {
                        enforcement: RuntimeBudgetEnforcement::Enforce,
                        default_workflow_budget_usd: 0.10,
                        ..Default::default()
                    },
                    RuntimeBudgetPolicy {
                        default_workflow_budget_usd: 0.10,
                        ..Default::default()
                    },
                    RuntimeBudgetPolicy {
                        enforcement: RuntimeBudgetEnforcement::Enforce,
                        unlimited: true,
                        default_workflow_budget_usd: 0.10,
                        ..Default::default()
                    },
                ] {
                    accounting_case(cost_only, None, oneshot, policy, None).await?;
                }
            }
        }
        Ok(())
    }
}
