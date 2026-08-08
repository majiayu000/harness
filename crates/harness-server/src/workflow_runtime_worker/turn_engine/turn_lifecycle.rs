use super::helpers::{
    emit_runtime_notification, mark_turn_failed, process_stream_item, RuntimeUsageContext,
};
use harness_core::agent::{AgentEvent, AgentRequest, StreamItem, TurnRequest};
use harness_core::config::agents::{AgentPermissionMode, SandboxMode};
use harness_core::config::stall_timeout::normalize_stall_timeout_secs;
use harness_core::error::HarnessError;
use harness_core::run_id::RunIdentity;
use harness_core::types::{ExecutionPhase, TurnId};
use harness_protocol::notifications::{Notification, RpcNotification};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

pub(super) fn bridge_agent_event(
    event: AgentEvent,
    output_buf: &mut String,
    emitted_agent_completion: &mut bool,
) -> Option<StreamItem> {
    match event {
        AgentEvent::EgressVerifiedAtDispatch => Some(StreamItem::EgressVerifiedAtDispatch),
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
    pub permission_mode: AgentPermissionMode,
    pub allowed_tools: Option<Vec<String>>,
    pub env_vars: HashMap<String, String>,
    pub runtime_usage: Option<RuntimeUsageContext>,
    /// Set when the agent confirms that its first-party egress proxy was
    /// established before dispatch. The marker is consumed here and is not
    /// persisted as a transcript item.
    pub egress_verified_at_dispatch: Option<Arc<AtomicBool>>,
    /// Stateful lease-lost signal (watch channel): when the owning runtime
    /// job lease is lost mid-turn, the turn interrupts the agent so the
    /// child process terminates and the workspace cleanup can run (GH-1877).
    pub lease_lost: Option<tokio::sync::watch::Receiver<bool>>,
}

pub(crate) async fn run_turn_lifecycle_with_options(
    server: Arc<crate::server::HarnessServer>,
    notify_tx: Option<crate::notify::NotifySender>,
    notification_tx: tokio::sync::broadcast::Sender<RpcNotification>,
    thread_id: harness_core::types::ThreadId,
    turn_id: TurnId,
    prompt: String,
    agent_name: String,
    mut options: TurnLifecycleOptions,
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
    match RunIdentity::mint_nested_env_vars(&mut options.env_vars) {
        Ok(identity) => {
            if let Some(context) = options.runtime_usage.as_mut() {
                context.agent_run_id = Some(identity.run_id);
            }
        }
        Err(err) => {
            tracing::warn!("failed to prepare agent run identity for runtime turn: {err}");
        }
    }

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
            &notify_tx,
            &notification_tx,
            &thread_id,
            &turn_id,
            msg,
        )
        .await;
        return;
    };
    if let Some(context) = options.runtime_usage.as_ref() {
        if let Err(error) = context.persist_agent_run_start(&turn_id).await {
            tracing::error!(
                runtime_job_id = %context.runtime_job_id,
                command_id = %context.command_id,
                workflow_id = %context.workflow_id,
                "failed to persist workflow runtime agent run start: {error}"
            );
        }
    }

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
    // Adapters may be control-only (interrupt/steer/approval side channel
    // only) or turn-executing (Codex: App Server JSON-RPC owns the full
    // turn). The strategy is selected at agent registration time.
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
            permission_mode: options.permission_mode,
            model: options.model.clone(),
            reasoning_effort: options.reasoning_effort.clone(),
            execution_phase: options.execution_phase,
            sandbox_mode: options.sandbox_mode,
            approval_policy: options.approval_policy.clone(),
            allowed_tools: options.allowed_tools.clone(),
            context: vec![],
            timeout_secs,
            env_vars: options.env_vars.clone(),
            capability_token: None,
        };
        Box::pin(async move { adapter_arc.start_turn(turn_req, event_tx).await })
    } else {
        let req = AgentRequest {
            prompt,
            project_root,
            permission_mode: options.permission_mode,
            model: options.model.clone(),
            reasoning_effort: options.reasoning_effort.clone(),
            execution_phase: options.execution_phase,
            sandbox_mode: options.sandbox_mode,
            approval_policy: options.approval_policy.clone(),
            allowed_tools: options.allowed_tools.clone(),
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
                    Some(StreamItem::EgressVerifiedAtDispatch) => {
                        last_activity = Instant::now();
                        if let Some(verified) = options.egress_verified_at_dispatch.as_ref() {
                            verified.store(true, Ordering::Release);
                        }
                    }
                    Some(item) => {
                        last_activity = Instant::now();
                        if let StreamItem::Error { message } = &item {
                            stream_error.get_or_insert_with(|| message.clone());
                        }
                        let budget_stop = process_stream_item(
                            &server,
                            &notify_tx,
                            &notification_tx,
                            options.runtime_usage.as_ref(),
                            &thread_id,
                            &turn_id,
                            item,
                        ).await;
                        // GH-1770 §4.3: an activity that already blew the
                        // workflow ceiling is precisely the case dispatch-time
                        // gating cannot catch, so the in-flight turn stops.
                        if let Some(stop) = budget_stop {
                            tracing::warn!(
                                thread_id = %thread_id,
                                turn_id = %turn_id,
                                workflow_id = %stop.workflow_id,
                                spent_usd = stop.spent_usd,
                                budget_usd = stop.budget_usd,
                                "workflow budget ceiling reached mid-turn; interrupting agent"
                            );
                            if let Some(adapter) = adapter_opt.as_ref() {
                                if let Err(error) = adapter.interrupt().await {
                                    tracing::warn!(
                                        thread_id = %thread_id,
                                        turn_id = %turn_id,
                                        "failed to interrupt agent after the budget ceiling: {error}"
                                    );
                                }
                            }
                            execution_result = Some(Err(HarnessError::AgentExecution(format!(
                                "Workflow {} spent {:.2} USD, reaching its {:.2} USD budget; turn interrupted.",
                                stop.workflow_id, stop.spent_usd, stop.budget_usd
                            ))));
                            break 'outer;
                        }
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
            _ = async {
                    match options.lease_lost.as_ref() {
                        Some(receiver) => {
                            let mut receiver = receiver.clone();
                            loop {
                                if receiver.changed().await.is_err() {
                                    return;
                                }
                                if *receiver.borrow() {
                                    return;
                                }
                            }
                        }
                        None => std::future::pending().await,
                    }
                }, if execution_result.is_none() && !stream_closed => {
                tracing::warn!(
                    thread_id = %thread_id,
                    turn_id = %turn_id,
                    "runtime job lease lost mid-turn; interrupting agent"
                );
                if let Some(adapter) = adapter_opt.as_ref() {
                    if let Err(error) = adapter.interrupt().await {
                        tracing::warn!(
                            thread_id = %thread_id,
                            turn_id = %turn_id,
                            "failed to interrupt agent after lease loss: {error}"
                        );
                    }
                }
                execution_result = Some(Err(HarnessError::AgentExecution(
                    "Runtime job lease was lost before the agent completed; turn interrupted.".to_string(),
                )));
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
                    }
                    mark_turn_failed(
                        &server,
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
            }
            mark_turn_failed(
                &server,
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
