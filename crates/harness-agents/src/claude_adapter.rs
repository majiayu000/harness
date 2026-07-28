use crate::provider_backpressure::{
    ProviderBackpressureGate, ProviderBackpressurePermit, ProviderPhase,
    PROVIDER_WAIT_HEARTBEAT_INTERVAL, PROVIDER_WAIT_INITIAL_HEARTBEAT_DELAY,
};
use async_trait::async_trait;
use harness_core::config::agents::SandboxMode;
use harness_core::{agent::AgentAdapter, agent::AgentEvent, agent::TurnRequest, types::TokenUsage};
use harness_observe::usage::parse_result_usage_metrics;
use harness_sandbox::SandboxSpec;
use serde_json::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tracing;

/// Streaming Claude Code adapter (L1-L2).
///
/// Spawns `claude --output-format stream-json -p <prompt>` and parses JSONL
/// events in realtime, mapping them to `AgentEvent`s.
pub struct ClaudeAdapter {
    cli_path: PathBuf,
    default_model: String,
    child: Arc<Mutex<Option<tokio::process::Child>>>,
    provider_gate: ProviderBackpressureGate,
}

impl ClaudeAdapter {
    pub fn new(cli_path: PathBuf, default_model: String) -> Self {
        Self {
            cli_path,
            default_model,
            child: Arc::new(Mutex::new(None)),
            provider_gate: ProviderBackpressureGate::disabled(),
        }
    }

    pub fn with_provider_backpressure_gate(mut self, gate: ProviderBackpressureGate) -> Self {
        self.provider_gate = gate;
        self
    }
}

#[async_trait]
impl AgentAdapter for ClaudeAdapter {
    fn name(&self) -> &str {
        "claude"
    }

    async fn start_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::Sender<AgentEvent>,
    ) -> harness_core::error::Result<()> {
        // Check token expiry before spawning.
        // See also: claude.rs — both files must stay in sync on this check.
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let prompt = req.claude_main_prompt();
        // Prompt MUST follow -p immediately: Claude CLI parses `-p <VALUE>`.
        let mut args = vec![
            OsString::from("-p"),
            OsString::from(prompt.as_ref()),
            OsString::from("--output-format"),
            OsString::from("stream-json"),
            OsString::from("--model"),
            OsString::from(model),
            OsString::from("--verbose"),
        ];
        if let Some(system_prompt) = req.claude_system_prompt() {
            args.push(OsString::from("--append-system-prompt"));
            args.push(OsString::from(system_prompt));
            args.push(OsString::from("--exclude-dynamic-system-prompt-sections"));
        }
        // Hard tool enforcement at the CLI boundary (issue #514):
        //   Full profile  (allowed_tools empty)   → --dangerously-skip-permissions
        //   Restricted profile (allowed_tools set) → --permission-mode bypassPermissions
        //                                             --allowedTools <comma-list>
        // The two flags are mutually exclusive in Claude CLI 2.1.70+.
        // See also: claude.rs base_args — both files must stay in sync on this split.
        if req.uses_dangerously_skip_permissions() {
            args.push(OsString::from("--dangerously-skip-permissions"));
        } else {
            args.push(OsString::from("--permission-mode"));
            args.push(OsString::from("bypassPermissions"));
            args.push(OsString::from("--allowedTools"));
            let tools = req.allowed_tools.as_deref().unwrap_or(&[]);
            args.push(OsString::from(tools.join(",")));
        }

        // Narrow sandbox write paths to token scope when present.
        // See also: claude.rs / codex_adapter.rs — all must stay in sync on this conversion.
        let sandbox_mode = req.sandbox_mode.unwrap_or(SandboxMode::DangerFullAccess);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(sandbox_mode, &req.project_root)
        };
        let mut spawn_env_vars = req.env_vars.clone();
        let run_identity = crate::resolve_agent_run_identity(&spawn_env_vars);
        run_identity.write_env_vars(&mut spawn_env_vars);
        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.cli_path,
                args: &args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox_spec,
                env_vars: &spawn_env_vars,
                forward_stdin: false,
            })?;

        let mut cmd = Command::new(&prepared_spawn.program);
        cmd.args(&prepared_spawn.args)
            .current_dir(&prepared_spawn.current_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        crate::set_process_group(&mut cmd);
        crate::spawn_contract::apply_process_env(&mut cmd, &prepared_spawn);

        let provider_permit =
            acquire_provider_permit_with_event_heartbeat(&self.provider_gate, &req, &tx).await?;
        tracing::debug!(
            phase = provider_permit.phase().label(),
            waited_ms = provider_permit.waited_ms(),
            "claude adapter admitted by provider gate"
        );

        let mut child = cmd.spawn().map_err(|error| {
            let message = crate::classify_missing_workspace_spawn_failure(
                &error,
                &req.project_root,
                format!("failed to spawn claude: {error}"),
            );
            harness_core::error::HarnessError::AgentExecution(message)
        })?;
        if let Some(pid) = child.id() {
            crate::write_provisional_agent_run_binding(
                &run_identity,
                "claude-code",
                pid,
                &prepared_spawn.current_dir,
            );
        }

        let stdout = child.stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution(
                "no stdout from claude process".into(),
            )
        })?;

        // Drain stderr concurrently to avoid pipe-buffer deadlock and capture
        // the error message when the process exits non-zero.
        let stderr_handle = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let Some(stderr) = stderr_handle else {
                return String::new();
            };
            let mut buf = String::new();
            if let Err(e) = tokio::io::AsyncReadExt::read_to_string(
                &mut tokio::io::BufReader::new(stderr),
                &mut buf,
            )
            .await
            {
                tracing::warn!("claude: failed to read stderr: {e}");
            }
            buf
        });

        // Store child handle for interrupt()
        {
            let mut guard = self.child.lock().await;
            *guard = Some(child);
        }

        if tx.send(AgentEvent::TurnStarted).await.is_err() {
            return Ok(());
        }

        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut output_buf = String::new();
        let mut turn_failed = false;

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            if parse_stream_json_result_failure(&line).is_some() {
                turn_failed = true;
            }
            let events = parse_stream_json_events(&line);
            if events.is_empty() {
                continue;
            }

            if let Some(usage) = parse_stream_json_usage(&line) {
                if tx.send(AgentEvent::TokenUsage { usage }).await.is_err() {
                    break;
                }
            }

            let mut channel_closed = false;
            for event in events {
                // Accumulate output text for TurnCompleted
                if let AgentEvent::MessageDelta { ref text } = event {
                    output_buf.push_str(text);
                }

                if tx.send(event).await.is_err() {
                    channel_closed = true;
                    break;
                }
            }
            if channel_closed {
                break;
            }
        }

        // Wait for process to finish and get exit status
        let exit_status = {
            let mut guard = self.child.lock().await;
            if let Some(ref mut child) = *guard {
                child
                    .wait()
                    .await
                    .map_err(|e| {
                        tracing::warn!("claude: failed to wait for child process: {e}");
                    })
                    .ok()
            } else {
                None
            }
        };

        let stderr_text = stderr_task.await.unwrap_or_default();
        if !stderr_text.is_empty() {
            tracing::warn!("claude stderr: {}", stderr_text.trim());
        }

        if let Some(status) = exit_status {
            if !status.success() {
                let stderr_suffix = if stderr_text.is_empty() {
                    String::new()
                } else {
                    // Keep last 500 chars of stderr for the error message.
                    let trimmed: String = stderr_text
                        .chars()
                        .rev()
                        .take(500)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!(": {}", trimmed.trim())
                };
                if let Err(e) = tx
                    .send(AgentEvent::Error {
                        message: format!("claude exited with {status}{stderr_suffix}"),
                    })
                    .await
                {
                    tracing::debug!("claude: event channel closed before error could be sent: {e}");
                }
            }
        }

        // A terminal result failure already surfaced as AgentEvent::Error; a
        // trailing TurnCompleted would contradict it, so skip it in that case.
        if !turn_failed {
            if let Err(e) = tx
                .send(AgentEvent::TurnCompleted { output: output_buf })
                .await
            {
                tracing::debug!("claude: event channel closed before turn completed: {e}");
            }
        }

        // Clean up child handle
        let mut guard = self.child.lock().await;
        *guard = None;

        Ok(())
    }

    async fn interrupt(&self) -> harness_core::error::Result<()> {
        let mut guard = self.child.lock().await;
        if let Some(ref mut child) = *guard {
            child.kill().await.map_err(|e| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed to kill claude process: {e}"
                ))
            })?;
        }
        Ok(())
    }

    async fn steer(&self, _text: String) -> harness_core::error::Result<()> {
        // Claude CLI is a one-shot process launched with `-p`.  It has no open
        // stdin channel for mid-turn injection, so live steering is not possible
        // without process restart.  Future interactive-mode support would require
        // a different spawning strategy.
        Err(harness_core::error::HarnessError::Unsupported(
            "Claude CLI does not support live steering: it is a one-shot process \
             launched with -p and has no stdin channel for mid-turn injection"
                .into(),
        ))
    }

    async fn respond_approval(
        &self,
        _id: String,
        _decision: harness_core::agent::ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        // Claude CLI runs with --dangerously-skip-permissions and auto-approves
        // all tool calls.  There is no approval gate protocol to respond to.
        Err(harness_core::error::HarnessError::Unsupported(
            "Claude CLI does not support approval responses: it runs with \
             --dangerously-skip-permissions and cannot receive mid-turn input"
                .into(),
        ))
    }
}

async fn acquire_provider_permit_with_event_heartbeat(
    gate: &ProviderBackpressureGate,
    req: &TurnRequest,
    tx: &mpsc::Sender<AgentEvent>,
) -> harness_core::error::Result<ProviderBackpressurePermit> {
    let prompt_chars = req.prompt.chars().count();
    let prompt_bytes = req.prompt.len();
    if !gate.is_enabled() {
        return gate
            .acquire(req.execution_phase, prompt_chars, prompt_bytes)
            .await;
    }

    let phase = ProviderPhase::from_execution_phase(req.execution_phase);
    let mut acquire = Box::pin(gate.acquire(req.execution_phase, prompt_chars, prompt_bytes));
    let mut heartbeat = Box::pin(tokio::time::sleep(PROVIDER_WAIT_INITIAL_HEARTBEAT_DELAY));
    loop {
        tokio::select! {
            permit = &mut acquire => return permit,
            _ = &mut heartbeat => {
                if tx.send(AgentEvent::Warning {
                    message: provider_wait_message(phase),
                }).await.is_err() {
                    return Err(harness_core::error::HarnessError::AgentExecution(
                        "agent event channel closed while waiting for Claude provider capacity".into(),
                    ));
                }
                heartbeat.as_mut().reset(tokio::time::Instant::now() + PROVIDER_WAIT_HEARTBEAT_INTERVAL);
            }
        }
    }
}

fn provider_wait_message(phase: ProviderPhase) -> String {
    format!(
        "Waiting for Claude provider capacity for {} phase",
        phase.label()
    )
}

/// Parse a single line of Claude Code `--output-format stream-json` output.
///
/// Returns the first event on the line; use [`parse_stream_json_events`] when a
/// line can carry several (e.g. an assistant message mixing text and tool_use
/// content blocks).
pub fn parse_stream_json_line(line: &str) -> Option<AgentEvent> {
    parse_stream_json_events(line).into_iter().next()
}

/// Parse a single line of Claude Code `--output-format stream-json` output into
/// every event it carries, in content-block order.
///
/// Returns an empty vec for unrecognized event types (forward compatibility).
pub fn parse_stream_json_events(line: &str) -> Vec<AgentEvent> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let Some(event_type) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };

    match event_type {
        "assistant" => v
            .get("message")
            .map(parse_assistant_events)
            .unwrap_or_default(),
        "tool_use" => {
            let Some(name) = v.get("name").and_then(Value::as_str) else {
                return Vec::new();
            };
            let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
            vec![AgentEvent::ToolCall {
                name: name.to_string(),
                input,
            }]
        }
        "tool_result" => vec![AgentEvent::ItemCompleted],
        "result" => match parse_result_failure_value(&v) {
            Some(message) => vec![AgentEvent::Error { message }],
            None => {
                let output = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                vec![AgentEvent::TurnCompleted { output }]
            }
        },
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error")
                .to_string();
            vec![AgentEvent::Error { message }]
        }
        _ => Vec::new(),
    }
}

/// Detect a terminal `result` event that reports failure (`is_error` or an
/// `error*` subtype). The Claude CLI emits these with exit code 0, so callers
/// must not rely on the process status to notice the failure.
pub(crate) fn parse_stream_json_result_failure(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    parse_result_failure_value(&v)
}

fn parse_result_failure_value(v: &Value) -> Option<String> {
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("");
    let is_error =
        v.get("is_error").and_then(Value::as_bool) == Some(true) || subtype.starts_with("error");
    if !is_error {
        return None;
    }

    let detail = v
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            v.get("error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    let subtype_label = if subtype.is_empty() {
        "unknown"
    } else {
        subtype
    };
    Some(match detail {
        Some(detail) => format!("claude result reported failure ({subtype_label}): {detail}"),
        None => format!("claude result reported failure ({subtype_label})"),
    })
}

fn parse_assistant_events(message: &Value) -> Vec<AgentEvent> {
    if let Some(text) = message.as_str() {
        return vec![AgentEvent::MessageDelta {
            text: text.to_string(),
        }];
    }

    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    let mut text_buf = String::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_buf.push_str(text);
                }
            }
            Some("tool_use") => {
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    if !text_buf.is_empty() {
                        events.push(AgentEvent::MessageDelta {
                            text: std::mem::take(&mut text_buf),
                        });
                    }
                    let input = block
                        .get("input")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    events.push(AgentEvent::ToolCall {
                        name: name.to_string(),
                        input,
                    });
                }
            }
            _ => {}
        }
    }
    if !text_buf.is_empty() {
        events.push(AgentEvent::MessageDelta { text: text_buf });
    }
    events
}

pub fn parse_stream_json_usage(line: &str) -> Option<TokenUsage> {
    let usage = parse_result_usage_metrics(line)?;

    Some(TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens(),
        cost_usd: 0.0,
    })
}

#[cfg(test)]
#[path = "claude_adapter_tests.rs"]
mod tests;
