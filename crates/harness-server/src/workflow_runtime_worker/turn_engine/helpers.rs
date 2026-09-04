pub(crate) use super::runtime_usage::{RuntimeUsageContext, TurnBudgetStop};
use harness_core::agent::StreamItem;
use harness_core::types::{Item, ThreadId, TurnId, TurnStatus};
use harness_protocol::{notifications::Notification, notifications::RpcNotification};

#[derive(Debug, Default)]
pub(crate) struct StreamCompletionState {
    output_buf: String,
    emitted_agent_completion: bool,
}

impl StreamCompletionState {
    pub(crate) fn normalize(&mut self, stream_item: StreamItem) -> Option<StreamItem> {
        match stream_item {
            StreamItem::MessageDelta { text } => {
                self.output_buf.push_str(&text);
                Some(StreamItem::MessageDelta { text })
            }
            StreamItem::ItemCompleted { item } => {
                if let Item::AgentReasoning { content } = &item {
                    self.output_buf.clear();
                    self.output_buf.push_str(content);
                    self.emitted_agent_completion = true;
                }
                Some(StreamItem::ItemCompleted { item })
            }
            // GH-1933: adapter diagnostics are non-terminal; log them and
            // surface them to the turn as warnings.
            StreamItem::Diagnostic { severity, message } => {
                match severity {
                    harness_core::agent::AgentDiagnosticSeverity::Warning => {
                        tracing::warn!(agent_diagnostic = true, "{message}");
                    }
                    harness_core::agent::AgentDiagnosticSeverity::Error => {
                        tracing::error!(
                            agent_diagnostic = true,
                            "non-terminal agent diagnostic: {message}"
                        );
                    }
                }
                Some(StreamItem::Warning { message })
            }
            StreamItem::TurnCompleted { output } => {
                if self.emitted_agent_completion {
                    self.output_buf.clear();
                    return None;
                }
                let content = if output.is_empty() {
                    std::mem::take(&mut self.output_buf)
                } else {
                    output
                };
                if content.is_empty() {
                    None
                } else {
                    self.emitted_agent_completion = true;
                    Some(StreamItem::ItemCompleted {
                        item: Item::AgentReasoning { content },
                    })
                }
            }
            other => Some(other),
        }
    }
}

pub(crate) fn emit_runtime_notification(
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    notification: Notification,
) {
    crate::notify::emit(notify_tx, notification.clone());
    let _ = notification_tx.send(RpcNotification::new(notification));
}

/// Returns a [`TurnBudgetStop`] when the streamed usage just persisted put the
/// workflow at or over its budget under `enforce`; the caller interrupts the
/// in-flight turn (GH-1770 §4.3).
pub(crate) async fn process_stream_item(
    server: &crate::server::HarnessServer,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    runtime_usage: Option<&RuntimeUsageContext>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    stream_item: StreamItem,
) -> Option<TurnBudgetStop> {
    let mut budget_stop = None;
    match stream_item {
        StreamItem::EgressVerifiedAtDispatch => {}
        StreamItem::ItemStarted { item } => {
            if let Err(err) = server
                .thread_manager
                .add_item(thread_id, turn_id, item.clone())
            {
                tracing::warn!("failed to append stream item_started to turn: {err}");
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ItemStarted {
                    turn_id: turn_id.clone(),
                    item,
                },
            );
        }
        StreamItem::ItemCompleted { item } => {
            if let Err(err) = server
                .thread_manager
                .add_item(thread_id, turn_id, item.clone())
            {
                tracing::warn!("failed to append stream item_completed to turn: {err}");
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ItemCompleted {
                    turn_id: turn_id.clone(),
                    item,
                },
            );
        }
        StreamItem::TokenUsage {
            usage,
            cost_usd_observed,
        } => {
            if let Err(err) =
                server
                    .thread_manager
                    .set_turn_token_usage(thread_id, turn_id, usage.clone())
            {
                tracing::warn!("failed to update turn token usage: {err}");
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::TokenUsageUpdated {
                    thread_id: thread_id.clone(),
                    usage: usage.clone(),
                },
            );
            if let Some(context) = runtime_usage {
                if let Err(error) = context
                    .persist_token_usage(turn_id, &usage, cost_usd_observed)
                    .await
                {
                    tracing::error!(
                        runtime_job_id = %context.runtime_job_id,
                        command_id = %context.command_id,
                        workflow_id = %context.workflow_id,
                        "failed to persist workflow runtime token usage: {error}"
                    );
                }
                // Evaluated after the persist above so the aggregate includes
                // the usage that may have crossed the ceiling. A failed check
                // must not silently disable the watchdog: it is logged at
                // error level and the completion ceiling remains the backstop.
                match context.budget_stop().await {
                    Ok(stop) => budget_stop = stop,
                    Err(error) => tracing::error!(
                        runtime_job_id = %context.runtime_job_id,
                        workflow_id = %context.workflow_id,
                        "failed to evaluate the mid-turn workflow budget ceiling: {error}"
                    ),
                }
            }
        }
        StreamItem::Error { message } => {
            if let Err(err) = server.thread_manager.add_item(
                thread_id,
                turn_id,
                harness_core::types::Item::error(message),
            ) {
                tracing::warn!("failed to append stream error item to turn: {err}");
            }
        }
        StreamItem::MessageDelta { text } => {
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::MessageDelta {
                    turn_id: turn_id.clone(),
                    text,
                },
            );
        }
        StreamItem::ToolOutputDelta { item_id, text } => {
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ToolOutputDelta {
                    turn_id: turn_id.clone(),
                    item_id,
                    text,
                },
            );
        }
        StreamItem::ApprovalRequest { id, command } => {
            if let Err(err) = server.thread_manager.add_item(
                thread_id,
                turn_id,
                harness_core::types::Item::ApprovalRequest {
                    id: Some(id.clone()),
                    action: command.clone(),
                    approved: None,
                },
            ) {
                tracing::warn!("failed to append approval request item to turn: {err}");
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ApprovalRequest {
                    turn_id: turn_id.clone(),
                    request_id: id,
                    command,
                },
            );
        }
        StreamItem::Warning { message } => {
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::Warning {
                    turn_id: turn_id.clone(),
                    message,
                },
            );
        }
        StreamItem::TurnCompleted { output } if !output.is_empty() => {
            let item = Item::AgentReasoning { content: output };
            if let Err(err) = server
                .thread_manager
                .add_item(thread_id, turn_id, item.clone())
            {
                tracing::warn!("failed to append stream turn_completed to turn: {err}");
            }
            emit_runtime_notification(
                notify_tx,
                notification_tx,
                Notification::ItemCompleted {
                    turn_id: turn_id.clone(),
                    item,
                },
            );
        }
        _ => {}
    }
    budget_stop
}

pub(crate) async fn mark_turn_failed(
    server: &crate::server::HarnessServer,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    error: String,
) {
    if let Err(err) = server.thread_manager.fail_turn(thread_id, turn_id) {
        tracing::warn!("failed to mark turn as failed: {err}");
    }
    emit_runtime_notification(
        notify_tx,
        notification_tx,
        Notification::TurnCompleted {
            turn_id: turn_id.clone(),
            status: TurnStatus::Failed,
            token_usage: harness_core::types::TokenUsage::default(),
        },
    );
    tracing::error!("turn failed: {error}");
}

pub(crate) async fn mark_turn_cancelled(
    server: &crate::server::HarnessServer,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    reason: String,
) {
    if let Err(err) = server.thread_manager.cancel_turn(thread_id, turn_id) {
        tracing::warn!("failed to mark turn as cancelled: {err}");
    }
    emit_runtime_notification(
        notify_tx,
        notification_tx,
        Notification::TurnCompleted {
            turn_id: turn_id.clone(),
            status: TurnStatus::Cancelled,
            token_usage: harness_core::types::TokenUsage::default(),
        },
    );
    tracing::info!("turn cancelled: {reason}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::{
        config::{
            workflow::{RuntimeBudgetEnforcement, RuntimeBudgetPolicy},
            HarnessConfig,
        },
        db::resolve_database_url,
        run_id::RunId,
        types::{AgentId, TokenUsage},
    };
    use harness_workflow::runtime::{
        RuntimeKind, WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject,
    };
    use std::str::FromStr;
    use std::sync::Arc;

    #[tokio::test]
    async fn mark_turn_cancelled_transitions_turn_status() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut config = HarnessConfig::default();
        config.server.project_root = dir.path().to_path_buf();
        let server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("codex"));
        let thread_id = server.thread_manager.start_thread(dir.path().to_path_buf());
        let turn_id = server.thread_manager.start_turn(
            &thread_id,
            "prompt".to_string(),
            AgentId::from_str("codex"),
        )?;
        let (notification_tx, _) = tokio::sync::broadcast::channel(16);

        mark_turn_cancelled(
            &server,
            &None,
            &notification_tx,
            &thread_id,
            &turn_id,
            "codex turn interrupted".to_string(),
        )
        .await;

        let turn = server
            .thread_manager
            .get_turn(&thread_id, &turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
        assert_eq!(turn.status, TurnStatus::Cancelled);
        Ok(())
    }

    #[tokio::test]
    async fn workflow_runtime_worker_token_usage_persists_runtime_usage() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = Arc::new(WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?);
        let mut config = HarnessConfig::default();
        config.server.project_root = dir.path().to_path_buf();
        let server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("codex"));
        let thread_id = server.thread_manager.start_thread(dir.path().to_path_buf());
        let turn_id = server.thread_manager.start_turn(
            &thread_id,
            "prompt".to_string(),
            AgentId::from_str("codex"),
        )?;
        let (notification_tx, _) = tokio::sync::broadcast::channel(16);
        let context = RuntimeUsageContext {
            store: store.clone(),
            runtime_job_id: "runtime-job-1".to_string(),
            command_id: "command-1".to_string(),
            workflow_id: "workflow-1".to_string(),
            agent_run_id: Some(RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd")?),
            runtime_kind: RuntimeKind::CodexExec,
            runtime_profile: "codex-default".to_string(),
            agent: "codex".to_string(),
            model: "gpt-5".to_string(),
            project: dir.path().to_string_lossy().into_owned(),
            task_id: Some("issue-1439".to_string()),
            candidate_group_id: Some("candidate-group".to_string()),
            candidate_id: Some("candidate-1".to_string()),
            candidate_index: Some(1),
            candidate_count: Some(2),
            budget_policy: RuntimeBudgetPolicy::default(),
        };

        process_stream_item(
            &server,
            &None,
            &notification_tx,
            Some(&context),
            &thread_id,
            &turn_id,
            StreamItem::TokenUsage {
                usage: TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 20,
                    cost_usd: 0.125,
                },
                cost_usd_observed: true,
            },
        )
        .await;

        let records = store
            .runtime_usage_between(
                chrono::Utc::now() - chrono::Duration::minutes(1),
                chrono::Utc::now(),
            )
            .await?;
        let turn = server
            .thread_manager
            .get_turn(&thread_id, &turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].runtime_job_id, "runtime-job-1");
        assert_eq!(
            records[0].agent_run_id.as_ref().map(RunId::as_str),
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6wd")
        );
        assert_eq!(records[0].metrics.input_tokens, 11);
        assert_eq!(records[0].metrics.output_tokens, 7);
        assert_eq!(records[0].metrics.total_tokens(), 20);
        assert_eq!(records[0].cost_usd_micros, 125_000);
        assert_eq!(records[0].candidate_id.as_deref(), Some("candidate-1"));
        assert_eq!(turn.token_usage.total_tokens, 20);
        Ok(())
    }

    struct WatchdogFixture {
        _dir: tempfile::TempDir,
        store: Arc<WorkflowRuntimeStore>,
        server: HarnessServer,
        thread_id: ThreadId,
        turn_id: TurnId,
        notification_tx: tokio::sync::broadcast::Sender<RpcNotification>,
    }

    /// The watchdog's shadow event is a workflow event, so its instance row
    /// must exist for the foreign key.
    async fn watchdog_fixture(workflow_id: &str) -> anyhow::Result<WatchdogFixture> {
        let dir = tempfile::tempdir()?;
        let store = Arc::new(WorkflowRuntimeStore::open(&dir.path().join("runtime")).await?);
        store
            .upsert_instance(
                &WorkflowInstance::new(
                    "github_issue_pr",
                    1,
                    "discovered",
                    WorkflowSubject::new("issue", "issue:1770"),
                )
                .with_id(workflow_id),
            )
            .await?;
        let mut config = HarnessConfig::default();
        config.server.project_root = dir.path().to_path_buf();
        let server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("codex"));
        let thread_id = server.thread_manager.start_thread(dir.path().to_path_buf());
        let turn_id = server.thread_manager.start_turn(
            &thread_id,
            "prompt".to_string(),
            AgentId::from_str("codex"),
        )?;
        let (notification_tx, _) = tokio::sync::broadcast::channel(16);
        Ok(WatchdogFixture {
            _dir: dir,
            store,
            server,
            thread_id,
            turn_id,
            notification_tx,
        })
    }

    fn watchdog_context(
        store: Arc<WorkflowRuntimeStore>,
        workflow_id: &str,
        budget_policy: RuntimeBudgetPolicy,
    ) -> RuntimeUsageContext {
        RuntimeUsageContext {
            store,
            runtime_job_id: format!("runtime-job-{workflow_id}"),
            command_id: format!("command-{workflow_id}"),
            workflow_id: workflow_id.to_string(),
            agent_run_id: None,
            runtime_kind: RuntimeKind::CodexExec,
            runtime_profile: "codex-default".to_string(),
            agent: "codex".to_string(),
            model: "gpt-5".to_string(),
            project: "/project-a".to_string(),
            task_id: None,
            candidate_group_id: None,
            candidate_id: None,
            candidate_index: None,
            candidate_count: None,
            budget_policy,
        }
    }

    fn watchdog_usage(cost_usd: f64) -> StreamItem {
        StreamItem::TokenUsage {
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cost_usd,
            },
            cost_usd_observed: true,
        }
    }

    #[tokio::test]
    async fn budget_watchdog_enforce_stops_turn_at_the_ceiling() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let fixture = watchdog_fixture("watchdog-enforce").await?;
        let context = watchdog_context(
            fixture.store.clone(),
            "watchdog-enforce",
            RuntimeBudgetPolicy {
                default_workflow_budget_usd: 0.10,
                enforcement: RuntimeBudgetEnforcement::Enforce,
                ..RuntimeBudgetPolicy::default()
            },
        );

        let stop = process_stream_item(
            &fixture.server,
            &None,
            &fixture.notification_tx,
            Some(&context),
            &fixture.thread_id,
            &fixture.turn_id,
            watchdog_usage(0.125),
        )
        .await
        .expect("crossing the ceiling mid-turn must stop the turn");

        assert_eq!(stop.workflow_id, "watchdog-enforce");
        assert_eq!(stop.spent_usd, 0.125);
        assert_eq!(stop.budget_usd, 0.10);
        Ok(())
    }

    #[tokio::test]
    async fn budget_watchdog_shadow_records_event_without_stopping() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let fixture = watchdog_fixture("watchdog-shadow").await?;
        let context = watchdog_context(
            fixture.store.clone(),
            "watchdog-shadow",
            RuntimeBudgetPolicy {
                default_workflow_budget_usd: 0.10,
                enforcement: RuntimeBudgetEnforcement::Shadow,
                ..RuntimeBudgetPolicy::default()
            },
        );

        let stop = process_stream_item(
            &fixture.server,
            &None,
            &fixture.notification_tx,
            Some(&context),
            &fixture.thread_id,
            &fixture.turn_id,
            watchdog_usage(0.125),
        )
        .await;

        assert!(stop.is_none(), "shadow mode must not stop the turn");
        let events = fixture.store.events_for("watchdog-shadow").await?;
        let shadow = events
            .iter()
            .find(|event| event.event_type == "BudgetShadowDecision")
            .expect("shadow mode records a BudgetShadowDecision event");
        assert_eq!(shadow.source, "workflow_runtime_turn_watchdog");
        assert_eq!(shadow.event["decision"], "would_interrupt");
        assert_eq!(shadow.event["spent_usd"], 0.125);
        assert_eq!(shadow.event["budget_usd"], 0.10);
        Ok(())
    }

    #[tokio::test]
    async fn budget_watchdog_leaves_under_budget_turns_running() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let fixture = watchdog_fixture("watchdog-under").await?;
        let context = watchdog_context(
            fixture.store.clone(),
            "watchdog-under",
            RuntimeBudgetPolicy {
                default_workflow_budget_usd: 15.0,
                enforcement: RuntimeBudgetEnforcement::Enforce,
                ..RuntimeBudgetPolicy::default()
            },
        );

        let stop = process_stream_item(
            &fixture.server,
            &None,
            &fixture.notification_tx,
            Some(&context),
            &fixture.thread_id,
            &fixture.turn_id,
            watchdog_usage(0.125),
        )
        .await;

        assert!(stop.is_none(), "an under-budget turn keeps running");
        assert!(
            fixture
                .store
                .events_for("watchdog-under")
                .await?
                .iter()
                .all(|event| event.event_type != "BudgetShadowDecision"),
            "an under-budget turn records no budget decision"
        );
        Ok(())
    }

    #[tokio::test]
    async fn budget_watchdog_unlimited_policy_never_stops() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let fixture = watchdog_fixture("watchdog-unlimited").await?;
        let context = watchdog_context(
            fixture.store.clone(),
            "watchdog-unlimited",
            RuntimeBudgetPolicy {
                default_workflow_budget_usd: 0.01,
                enforcement: RuntimeBudgetEnforcement::Enforce,
                unlimited: true,
                ..RuntimeBudgetPolicy::default()
            },
        );

        let stop = process_stream_item(
            &fixture.server,
            &None,
            &fixture.notification_tx,
            Some(&context),
            &fixture.thread_id,
            &fixture.turn_id,
            watchdog_usage(5.0),
        )
        .await;

        assert!(stop.is_none(), "unlimited opt-out disables the watchdog");
        Ok(())
    }
}
