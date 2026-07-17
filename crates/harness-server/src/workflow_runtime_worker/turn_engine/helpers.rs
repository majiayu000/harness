use harness_core::agent::StreamItem;
use harness_core::types::{ThreadId, TokenUsage, TurnId, TurnStatus};
use harness_protocol::{notifications::Notification, notifications::RpcNotification};
use harness_workflow::runtime::{
    RuntimeKind, RuntimeUsageMetrics, RuntimeUsageUpsert, RuntimeUsageUpsertOutcome,
    WorkflowRuntimeStore,
};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct RuntimeUsageContext {
    pub(crate) store: Arc<WorkflowRuntimeStore>,
    pub(crate) runtime_job_id: String,
    pub(crate) command_id: String,
    pub(crate) workflow_id: String,
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
    async fn persist_token_usage(
        &self,
        turn_id: &TurnId,
        usage: &TokenUsage,
    ) -> anyhow::Result<()> {
        match self
            .store
            .upsert_runtime_usage(&RuntimeUsageUpsert {
                runtime_job_id: self.runtime_job_id.clone(),
                command_id: self.command_id.clone(),
                workflow_id: self.workflow_id.clone(),
                turn_id: Some(turn_id.as_str().to_string()),
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
                metrics: RuntimeUsageMetrics::from_token_usage(usage),
                reported_at: chrono::Utc::now(),
            })
            .await?
        {
            RuntimeUsageUpsertOutcome::SkippedZeroUsage => {}
            RuntimeUsageUpsertOutcome::Persisted => {}
        }
        Ok(())
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

pub(crate) async fn process_stream_item(
    server: &crate::server::HarnessServer,
    notify_tx: &Option<crate::notify::NotifySender>,
    notification_tx: &tokio::sync::broadcast::Sender<RpcNotification>,
    runtime_usage: Option<&RuntimeUsageContext>,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    stream_item: StreamItem,
) {
    match stream_item {
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
        StreamItem::TokenUsage { usage } => {
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
                if let Err(error) = context.persist_token_usage(turn_id, &usage).await {
                    tracing::error!(
                        runtime_job_id = %context.runtime_job_id,
                        command_id = %context.command_id,
                        workflow_id = %context.workflow_id,
                        "failed to persist workflow runtime token usage: {error}"
                    );
                }
            }
        }
        StreamItem::Error { message } => {
            if let Err(err) = server.thread_manager.add_item(
                thread_id,
                turn_id,
                harness_core::types::Item::Error { code: -1, message },
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
        _ => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::{
        config::HarnessConfig,
        db::resolve_database_url,
        types::{AgentId, TokenUsage},
    };
    use harness_workflow::runtime::{RuntimeKind, WorkflowRuntimeStore};

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
                    cost_usd: 0.0,
                },
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
        assert_eq!(records[0].metrics.input_tokens, 11);
        assert_eq!(records[0].metrics.output_tokens, 7);
        assert_eq!(records[0].metrics.total_tokens(), 20);
        assert_eq!(records[0].candidate_id.as_deref(), Some("candidate-1"));
        assert_eq!(turn.token_usage.total_tokens, 20);
        Ok(())
    }
}
