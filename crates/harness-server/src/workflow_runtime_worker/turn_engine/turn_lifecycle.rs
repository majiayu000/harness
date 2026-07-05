use super::helpers::{
    emit_runtime_notification, mark_turn_failed, persist_runtime_thread, process_stream_item,
    RuntimeUsageContext,
};
use harness_core::agent::{AgentEvent, AgentRequest, StreamItem, TurnRequest};
use harness_core::config::agents::SandboxMode;
use harness_core::config::stall_timeout::normalize_stall_timeout_secs;
use harness_core::error::HarnessError;
use harness_core::types::{ExecutionPhase, TurnId};
use harness_protocol::notifications::{Notification, RpcNotification};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

fn bridge_agent_event(
    event: AgentEvent,
    output_buf: &mut String,
    emitted_agent_completion: &mut bool,
) -> Option<StreamItem> {
    match event {
        AgentEvent::ItemStartedPayload { item } => Some(StreamItem::ItemStarted { item }),
        AgentEvent::MessageDelta { text } => {
            output_buf.push_str(&text);
            Some(StreamItem::MessageDelta { text })
        }
        AgentEvent::ToolOutputDelta { item_id, text } => {
            Some(StreamItem::ToolOutputDelta { item_id, text })
        }
        AgentEvent::ApprovalRequest { id, command } => {
            Some(StreamItem::ApprovalRequest { id, command })
        }
        AgentEvent::ItemCompletedPayload { item } => {
            if let harness_core::types::Item::AgentReasoning { content } = &item {
                output_buf.clear();
                output_buf.push_str(content);
                *emitted_agent_completion = true;
            }
            Some(StreamItem::ItemCompleted { item })
        }
        AgentEvent::TokenUsage { usage } => Some(StreamItem::TokenUsage { usage }),
        AgentEvent::Warning { message } => Some(StreamItem::Warning { message }),
        AgentEvent::Error { message } => Some(StreamItem::Error { message }),
        AgentEvent::TurnCompleted { output } => {
            if *emitted_agent_completion {
                output_buf.clear();
                return None;
            }
            let content = if output.is_empty() {
                std::mem::take(output_buf)
            } else {
                output
            };
            Some(StreamItem::ItemCompleted {
                item: harness_core::types::Item::AgentReasoning { content },
            })
        }
        _ => None,
    }
}

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
    run_turn_lifecycle_with_options(
        server,
        thread_db,
        notify_tx,
        notification_tx,
        thread_id,
        turn_id,
        prompt,
        agent_name,
        TurnLifecycleOptions::default(),
    )
    .await;
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnLifecycleOptions {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub execution_phase: Option<ExecutionPhase>,
    pub sandbox_mode: Option<SandboxMode>,
    pub approval_policy: Option<String>,
    pub timeout_secs: Option<u64>,
    pub stall_timeout_secs: Option<u64>,
    pub force_code_agent: bool,
    pub env_vars: HashMap<String, String>,
    pub runtime_usage: Option<RuntimeUsageContext>,
}

pub(crate) async fn run_turn_lifecycle_with_options(
    server: Arc<crate::server::HarnessServer>,
    thread_db: Option<crate::thread_db::ThreadDb>,
    notify_tx: Option<crate::notify::NotifySender>,
    notification_tx: tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: harness_core::types::ThreadId,
    turn_id: TurnId,
    prompt: String,
    agent_name: String,
    options: TurnLifecycleOptions,
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

    // Prefer the per-turn execution adapter when one exists. Stateful
    // turn-executing adapters must not be shared across concurrent turns; the
    // registry may return a fresh adapter instance here.
    let execution_adapter = if options.force_code_agent {
        None
    } else {
        server.agent_registry.turn_execution_adapter(&agent_name)
    };
    let adapter_opt = if options.force_code_agent {
        None
    } else {
        execution_adapter
            .clone()
            .or_else(|| server.agent_registry.get_adapter(&agent_name))
    };

    // Register as live adapter (RAII guard for cleanup on turn exit).
    // Adapters may be control-only (Claude: interrupt/steer/approval side
    // channel only) or turn-executing (Codex: App Server JSON-RPC owns the
    // full turn). The strategy is selected at agent registration time.
    let _adapter_guard = adapter_opt.as_ref().map(|adapter_arc| {
        server
            .thread_manager
            .register_active_adapter(&turn_id, adapter_arc.clone());
        AdapterGuard {
            server: server.clone(),
            turn_id: turn_id.clone(),
        }
    });

    let timeout_secs = options.timeout_secs.map(|secs| secs.max(1));
    let stall_normalization = normalize_stall_timeout_secs(
        options
            .stall_timeout_secs
            .unwrap_or(server.config.concurrency.stall_timeout_secs),
        timeout_secs,
    );
    if stall_normalization.was_adjusted() {
        tracing::warn!(
            thread_id = %thread_id,
            turn_id = %turn_id,
            requested_stall_timeout_secs = stall_normalization.requested_secs,
            stall_timeout_secs = stall_normalization.effective_secs,
            timeout_secs = ?stall_normalization.wall_clock_timeout_secs,
            "agent stream stall timeout adjusted"
        );
    }
    let stall_timeout = Duration::from_secs(stall_normalization.effective_secs);
    let stall_timeout_enabled = timeout_secs
        .map(|timeout_secs| stall_normalization.effective_secs < timeout_secs)
        .unwrap_or(true);
    tracing::debug!(
        thread_id = %thread_id,
        turn_id = %turn_id,
        stall_timeout_secs = stall_timeout.as_secs(),
        timeout_secs = ?timeout_secs,
        stall_timeout_enabled,
        "starting agent turn with stall timeout"
    );
    let (stream_tx, mut stream_rx) = mpsc::channel(128);

    // Use the adapter for turn execution only when its registered strategy says
    // it owns the full lifecycle. This is the strategy pattern boundary between
    // Codex's App Server adapter and Claude's control-only adapter; avoid
    // branching on agent names here.
    let mut execution: std::pin::Pin<
        Box<dyn std::future::Future<Output = harness_core::error::Result<()>> + Send>,
    > = if let Some(adapter_arc) = execution_adapter {
        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(128);
        // Move stream_tx into the bridge task so dropping it closes stream_rx.
        let bridge_tx = stream_tx;
        tokio::spawn(async move {
            let mut output_buf = String::new();
            let mut emitted_agent_completion = false;
            while let Some(event) = event_rx.recv().await {
                let maybe_item =
                    bridge_agent_event(event, &mut output_buf, &mut emitted_agent_completion);
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
            prompt_layers: None,
            project_root,
            model: options.model.clone(),
            reasoning_effort: options.reasoning_effort.clone(),
            execution_phase: options.execution_phase,
            sandbox_mode: options.sandbox_mode,
            approval_policy: options.approval_policy.clone(),
            allowed_tools: vec![],
            context: vec![],
            timeout_secs,
            capability_token: None,
        };
        Box::pin(async move { adapter_arc.start_turn(turn_req, event_tx).await })
    } else {
        let req = AgentRequest {
            prompt,
            project_root,
            model: options.model.clone(),
            reasoning_effort: options.reasoning_effort.clone(),
            execution_phase: options.execution_phase,
            sandbox_mode: options.sandbox_mode,
            approval_policy: options.approval_policy.clone(),
            env_vars: options.env_vars.clone(),
            ..Default::default()
        };
        Box::pin(agent.execute_stream(req, stream_tx))
    };
    let mut stream_closed = false;
    let mut execution_result: Option<harness_core::error::Result<()>> = None;
    let mut stream_error: Option<String> = None;
    let mut last_activity = Instant::now();
    let execution_deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));
    let execution_timeout = async {
        if let Some(deadline) = execution_deadline {
            tokio::time::sleep_until(deadline).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(execution_timeout);

    'outer: while execution_result.is_none() || !stream_closed {
        tokio::select! {
            result = &mut execution, if execution_result.is_none() => {
                execution_result = Some(result);
            }
            incoming = stream_rx.recv(), if !stream_closed => {
                match incoming {
                    Some(item) => {
                        last_activity = Instant::now();
                        if let StreamItem::Error { message } = &item {
                            stream_error.get_or_insert_with(|| message.clone());
                        }
                        process_stream_item(
                            &server,
                            &thread_db,
                            &notify_tx,
                            &notification_tx,
                            options.runtime_usage.as_ref(),
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
            _ = tokio::time::sleep_until(last_activity + stall_timeout), if stall_timeout_enabled && execution_result.is_none() => {
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
            _ = &mut execution_timeout, if execution_result.is_none() => {
                let timeout_secs = timeout_secs.unwrap_or(1);
                tracing::warn!(
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    timeout_secs,
                    "agent turn execution timeout reached"
                );
                execution_result = Some(Err(HarnessError::AgentExecution(format!(
                    "Agent turn timed out after {timeout_secs}s"
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
        Ok(()) => {
            if let Some(error_msg) = stream_error {
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
                return;
            }
            match server.thread_manager.complete_turn(&thread_id, &turn_id) {
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
            }
        }
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

#[cfg(test)]
mod tests {
    use super::{bridge_agent_event, run_turn_lifecycle_with_options, TurnLifecycleOptions};
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::{AdapterExecutionStrategy, AgentRegistry};
    use harness_core::agent::{
        AgentAdapter, AgentEvent, AgentRequest, AgentResponse, CodeAgent, StreamItem, TurnRequest,
    };
    use harness_core::config::HarnessConfig;
    use harness_core::error::HarnessError;
    use harness_core::types::{AgentId, Capability, Item, TokenUsage, TurnId, TurnStatus};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::mpsc;

    struct CountingAgent {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CodeAgent for CountingAgent {
        fn name(&self) -> &str {
            "codex"
        }

        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Read]
        }

        async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
            Ok(AgentResponse {
                output: "ok".to_string(),
                stderr: String::new(),
                items: Vec::new(),
                token_usage: TokenUsage::default(),
                model: "codex".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: AgentRequest,
            tx: mpsc::Sender<StreamItem>,
        ) -> harness_core::error::Result<()> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            tx.send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: "agent stream done".to_string(),
                },
            })
            .await
            .map_err(|error| HarnessError::AgentExecution(format!("stream closed: {error}")))?;
            tx.send(StreamItem::Done)
                .await
                .map_err(|error| HarnessError::AgentExecution(format!("stream closed: {error}")))?;
            Ok(())
        }
    }

    struct CountingAdapter {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl AgentAdapter for CountingAdapter {
        fn name(&self) -> &str {
            "codex"
        }

        async fn start_turn(
            &self,
            _req: TurnRequest,
            tx: mpsc::Sender<AgentEvent>,
        ) -> harness_core::error::Result<()> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            tx.send(AgentEvent::TurnCompleted {
                output: "adapter done".to_string(),
            })
            .await
            .map_err(|error| HarnessError::AgentExecution(format!("adapter closed: {error}")))?;
            Ok(())
        }

        async fn interrupt(&self) -> harness_core::error::Result<()> {
            Ok(())
        }
    }

    fn server_with_codex_counts(
        root: &std::path::Path,
        agent_calls: Arc<AtomicUsize>,
        adapter_calls: Arc<AtomicUsize>,
    ) -> anyhow::Result<Arc<HarnessServer>> {
        let mut config = HarnessConfig::default();
        config.server.project_root = root.to_path_buf();
        config.agents.default_agent = "codex".to_string();

        let mut registry = AgentRegistry::new("codex");
        registry.register("codex", Arc::new(CountingAgent { calls: agent_calls }));
        let adapter_calls_for_factory = adapter_calls.clone();
        registry
            .register_adapter_factory_with_strategy(
                "codex",
                move || {
                    Arc::new(CountingAdapter {
                        calls: adapter_calls_for_factory.clone(),
                    })
                },
                AdapterExecutionStrategy::ExecuteTurns,
            )
            .map_err(|error| anyhow::anyhow!("{error}"))?;

        Ok(Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            registry,
        )))
    }

    fn start_test_turn(server: &HarnessServer, root: &std::path::Path) -> anyhow::Result<TurnId> {
        let thread_id = server.thread_manager.start_thread(root.to_path_buf());
        server
            .thread_manager
            .start_turn(&thread_id, "prompt".to_string(), AgentId::from_str("codex"))
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    async fn run_test_turn(
        server: Arc<HarnessServer>,
        root: &std::path::Path,
        turn_id: TurnId,
        options: TurnLifecycleOptions,
    ) -> anyhow::Result<()> {
        let thread_id = server
            .thread_manager
            .find_thread_for_turn(&turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
        let (notification_tx, _) = tokio::sync::broadcast::channel(16);
        run_turn_lifecycle_with_options(
            server,
            None,
            None,
            notification_tx,
            thread_id,
            turn_id,
            "prompt".to_string(),
            "codex".to_string(),
            options,
        )
        .await;
        anyhow::ensure!(root.exists(), "test root should still exist");
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_uses_registered_turn_adapter_by_default() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let server =
            server_with_codex_counts(root.path(), agent_calls.clone(), adapter_calls.clone())?;
        let turn_id = start_test_turn(&server, root.path())?;

        run_test_turn(
            server.clone(),
            root.path(),
            turn_id.clone(),
            TurnLifecycleOptions::default(),
        )
        .await?;

        assert_eq!(agent_calls.load(Ordering::Acquire), 0);
        assert_eq!(adapter_calls.load(Ordering::Acquire), 1);
        let thread_id = server
            .thread_manager
            .find_thread_for_turn(&turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
        let turn = server
            .thread_manager
            .get_turn(&thread_id, &turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
        assert_eq!(turn.status, TurnStatus::Completed);
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_force_code_agent_bypasses_turn_adapter() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let agent_calls = Arc::new(AtomicUsize::new(0));
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let server =
            server_with_codex_counts(root.path(), agent_calls.clone(), adapter_calls.clone())?;
        let turn_id = start_test_turn(&server, root.path())?;

        run_test_turn(
            server.clone(),
            root.path(),
            turn_id.clone(),
            TurnLifecycleOptions {
                force_code_agent: true,
                ..TurnLifecycleOptions::default()
            },
        )
        .await?;

        assert_eq!(agent_calls.load(Ordering::Acquire), 1);
        assert_eq!(adapter_calls.load(Ordering::Acquire), 0);
        let thread_id = server
            .thread_manager
            .find_thread_for_turn(&turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should belong to a thread"))?;
        let turn = server
            .thread_manager
            .get_turn(&thread_id, &turn_id)
            .ok_or_else(|| anyhow::anyhow!("turn should exist"))?;
        assert_eq!(turn.status, TurnStatus::Completed);
        Ok(())
    }

    #[test]
    fn bridge_preserves_warning_and_token_usage_events() {
        let mut output_buf = String::new();
        let mut warning_completion = false;
        let mut usage_completion = false;

        let warning = bridge_agent_event(
            AgentEvent::Warning {
                message: "careful".into(),
            },
            &mut output_buf,
            &mut warning_completion,
        );
        let usage = bridge_agent_event(
            AgentEvent::TokenUsage {
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    total_tokens: 3,
                    cost_usd: 0.0,
                },
            },
            &mut output_buf,
            &mut usage_completion,
        );

        assert_eq!(
            warning,
            Some(StreamItem::Warning {
                message: "careful".into()
            })
        );
        assert_eq!(
            usage,
            Some(StreamItem::TokenUsage {
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    total_tokens: 3,
                    cost_usd: 0.0,
                }
            })
        );
    }

    #[test]
    fn bridge_uses_buffered_output_when_turn_completed_payload_is_empty() {
        let mut output_buf = String::new();
        let mut emitted_agent_completion = false;
        let _ = bridge_agent_event(
            AgentEvent::MessageDelta {
                text: "hello".into(),
            },
            &mut output_buf,
            &mut emitted_agent_completion,
        );
        let completed = bridge_agent_event(
            AgentEvent::TurnCompleted {
                output: String::new(),
            },
            &mut output_buf,
            &mut emitted_agent_completion,
        );

        assert_eq!(
            completed,
            Some(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: "hello".into()
                }
            })
        );
        assert!(output_buf.is_empty());
    }

    #[test]
    fn bridge_suppresses_duplicate_turn_completed_after_agent_message_completion() {
        let mut output_buf = String::new();
        let mut emitted_agent_completion = false;
        let item_completed = bridge_agent_event(
            AgentEvent::ItemCompletedPayload {
                item: Item::AgentReasoning {
                    content: "done".into(),
                },
            },
            &mut output_buf,
            &mut emitted_agent_completion,
        );
        let turn_completed = bridge_agent_event(
            AgentEvent::TurnCompleted {
                output: "done".into(),
            },
            &mut output_buf,
            &mut emitted_agent_completion,
        );

        assert_eq!(
            item_completed,
            Some(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: "done".into()
                }
            })
        );
        assert!(emitted_agent_completion);
        assert_eq!(turn_completed, None);
        assert!(output_buf.is_empty());
    }
}
