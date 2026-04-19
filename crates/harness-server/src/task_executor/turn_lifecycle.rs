use super::helpers::{
    emit_runtime_notification, mark_turn_failed, persist_runtime_thread, process_stream_item,
};
use harness_core::agent::{AgentEvent, AgentRequest, StreamItem, TurnRequest};
use harness_core::error::HarnessError;
use harness_core::types::TurnId;
use harness_protocol::notifications::{Notification, RpcNotification};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

pub(crate) async fn run_turn_lifecycle(
    server: Arc<crate::server::HarnessServer>,
    thread_db: Option<crate::thread_db::ThreadDb>,
    notify_tx: Option<crate::notify::NotifySender>,
    notification_tx: tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: harness_core::types::ThreadId,
    turn_id: TurnId,
    prompt: String,
    agent_name: String,
) {
    let Some(project_root) = server
        .thread_manager
        .get_thread(&thread_id)
        .map(|thread| thread.project_root)
    else {
        tracing::warn!(
            "run_turn_lifecycle skipped because thread {} no longer exists",
            thread_id
        );
        return;
    };

    let Some(agent) = server.agent_registry.get(&agent_name) else {
        let msg = format!("agent `{agent_name}` not found in registry");
        if let Err(e) = server.thread_manager.add_item(
            &thread_id,
            &turn_id,
            harness_core::types::Item::Error {
                code: -1,
                message: msg.clone(),
            },
        ) {
            tracing::warn!("failed to add agent-not-found error item: {e}");
        }
        mark_turn_failed(
            &server,
            &thread_db,
            &notify_tx,
            &notification_tx,
            &thread_id,
            &turn_id,
            msg,
        )
        .await;
        return;
    };

    // RAII guard: ensures the adapter is deregistered when the turn scope exits,
    // even if the task is cancelled before reaching the end of this function.
    struct AdapterGuard {
        server: Arc<crate::server::HarnessServer>,
        turn_id: TurnId,
    }
    impl Drop for AdapterGuard {
        fn drop(&mut self) {
            self.server
                .thread_manager
                .deregister_active_adapter(&self.turn_id);
        }
    }

    // Get the adapter for this agent, if any.
    let adapter_opt = server.agent_registry.get_adapter(&agent_name);

    // Register as live adapter (RAII guard for cleanup on turn exit).
    // When an adapter is available, execution goes through adapter.start_turn()
    // (see below), so the adapter's stdin is initialized before any
    // steer/respond_approval call arrives.
    let _adapter_guard = adapter_opt.as_ref().map(|adapter_arc| {
        server
            .thread_manager
            .register_active_adapter(&turn_id, adapter_arc.clone());
        AdapterGuard {
            server: server.clone(),
            turn_id: turn_id.clone(),
        }
    });

    let stall_timeout = Duration::from_secs(server.config.concurrency.stall_timeout_secs);
    let (stream_tx, mut stream_rx) = mpsc::channel(128);

    // Only Codex uses the adapter for turn execution (initializes stdin so that
    // subsequent steer/respond_approval calls can find a live process).
    // Other adapters (e.g., ClaudeAdapter) exist for interrupt/steer only and must
    // not override the configured CodeAgent execution path — ClaudeCodeAgent carries
    // config-level settings (model, sandbox) that ClaudeAdapter does not replicate.
    let execution_adapter = adapter_opt
        .as_ref()
        .filter(|a| a.name() == "codex")
        .cloned();
    let mut execution: std::pin::Pin<
        Box<dyn std::future::Future<Output = harness_core::error::Result<()>> + Send>,
    > = if let Some(adapter_arc) = execution_adapter {
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(128);
        // Move stream_tx into the bridge task so dropping it closes stream_rx.
        let bridge_tx = stream_tx;
        tokio::spawn(async move {
            let mut output_buf = String::new();
            while let Some(event) = event_rx.recv().await {
                let maybe_item: Option<StreamItem> = match event {
                    AgentEvent::MessageDelta { ref text } => {
                        output_buf.push_str(text);
                        Some(StreamItem::MessageDelta { text: text.clone() })
                    }
                    AgentEvent::ApprovalRequest { id, command } => {
                        Some(StreamItem::ApprovalRequest { id, command })
                    }
                    AgentEvent::Error { message } => Some(StreamItem::Error { message }),
                    AgentEvent::TurnCompleted { output } => {
                        let content = if output.is_empty() {
                            std::mem::take(&mut output_buf)
                        } else {
                            output
                        };
                        Some(StreamItem::ItemCompleted {
                            item: harness_core::types::Item::AgentReasoning { content },
                        })
                    }
                    _ => None,
                };
                if let Some(item) = maybe_item {
                    if bridge_tx.send(item).await.is_err() {
                        return;
                    }
                }
            }
            // event_rx closed → adapter done; dropping bridge_tx closes stream_rx.
        });
        let turn_req = TurnRequest {
            prompt,
            project_root,
            model: None,
            allowed_tools: vec![],
            context: vec![],
            timeout_secs: None,
            capability_token: None,
        };
        Box::pin(async move { adapter_arc.start_turn(turn_req, event_tx).await })
    } else {
        let req = AgentRequest {
            prompt,
            project_root,
            ..Default::default()
        };
        Box::pin(agent.execute_stream(req, stream_tx))
    };
    let mut stream_closed = false;
    let mut execution_result: Option<harness_core::error::Result<()>> = None;
    let mut last_activity = Instant::now();

    'outer: while execution_result.is_none() || !stream_closed {
        tokio::select! {
            result = &mut execution, if execution_result.is_none() => {
                execution_result = Some(result);
            }
            incoming = stream_rx.recv(), if !stream_closed => {
                match incoming {
                    Some(item) => {
                        last_activity = Instant::now();
                        process_stream_item(
                            &server,
                            &thread_db,
                            &notify_tx,
                            &notification_tx,
                            &thread_id,
                            &turn_id,
                            item,
                        ).await;
                    }
                    None => {
                        stream_closed = true;
                    }
                }
            }
            _ = tokio::time::sleep_until(last_activity + stall_timeout) => {
                let elapsed = last_activity.elapsed();
                tracing::warn!(
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    elapsed_secs = elapsed.as_secs(),
                    "agent stream stall detected; no output for {}s",
                    stall_timeout.as_secs()
                );
                // Store the stall reason as the execution result so the Err branch
                // below appends a stall-specific Item::Error before marking failed.
                execution_result = Some(Err(HarnessError::AgentExecution(format!(
                    "Agent stream stalled: no output for {}s",
                    stall_timeout.as_secs()
                ))));
                break 'outer;
            }
        }
    }

    match execution_result.unwrap_or_else(|| {
        Err(harness_core::error::HarnessError::AgentExecution(
            "turn execution ended without agent result".to_string(),
        ))
    }) {
        Ok(()) => match server.thread_manager.complete_turn(&thread_id, &turn_id) {
            Ok(Some(usage)) => {
                persist_runtime_thread(&thread_db, &server, &thread_id).await;
                emit_runtime_notification(
                    &notify_tx,
                    &notification_tx,
                    Notification::TurnCompleted {
                        turn_id: turn_id.clone(),
                        status: harness_core::types::TurnStatus::Completed,
                        token_usage: usage,
                    },
                );
            }
            Ok(None) => {}
            Err(err) => {
                let error_msg = err.to_string();
                tracing::error!(
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    "failed to complete turn after execution: {error_msg}"
                );
                if let Err(e) = server.thread_manager.add_item(
                    &thread_id,
                    &turn_id,
                    harness_core::types::Item::Error {
                        code: -1,
                        message: format!("Failed to complete turn: {error_msg}"),
                    },
                ) {
                    tracing::warn!("failed to add error item to turn: {e}");
                } else {
                    persist_runtime_thread(&thread_db, &server, &thread_id).await;
                }
                mark_turn_failed(
                    &server,
                    &thread_db,
                    &notify_tx,
                    &notification_tx,
                    &thread_id,
                    &turn_id,
                    error_msg,
                )
                .await;
            }
        },
        Err(err) => {
            let error_msg = err.to_string();
            if let Err(e) = server.thread_manager.add_item(
                &thread_id,
                &turn_id,
                harness_core::types::Item::Error {
                    code: -1,
                    message: error_msg.clone(),
                },
            ) {
                tracing::warn!("failed to add error item to turn: {e}");
            } else {
                persist_runtime_thread(&thread_db, &server, &thread_id).await;
            }
            mark_turn_failed(
                &server,
                &thread_db,
                &notify_tx,
                &notification_tx,
                &thread_id,
                &turn_id,
                error_msg,
            )
            .await;
        }
    }
}
