use crate::streaming::{
    capture_agent_stderr_diagnostics, captured_stderr_tail, log_captured_stderr_diagnostics,
    send_stream_item,
};
use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::agents::{OpenCodeAgentConfig, SandboxMode};
use harness_core::types::{Capability, Item, TokenUsage};
use harness_sandbox::SandboxSpec;
use serde_json::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::ChildStdout;

/// Environment variable holding an inlined JSON permissions config.
///
/// opencode merges this on top of the user/global config, mirroring the
/// `AgentRequest::allowed_tools` restriction surface.
const OPENCODE_PERMISSION_ENV: &str = "OPENCODE_PERMISSION";

#[derive(Debug, Clone, PartialEq)]
pub enum OpenCodeRunEvent {
    Text {
        text: String,
    },
    ToolUse {
        command: String,
        exit_code: Option<i32>,
        output: String,
    },
    StepFinish {
        reason: String,
        tokens: TokenUsage,
    },
}

/// Parse one line of `opencode run --format json` output.
///
/// Emitted line shapes (verified against opencode 1.18.14):
/// - `{"type":"step_start","part":{...}}`
/// - `{"type":"text","part":{"type":"text","text":"..."}}`
/// - `{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{...}}}`
/// - `{"type":"step_finish","part":{"reason":"stop","tokens":{...}}}`
pub fn parse_opencode_run_line(line: &str) -> Option<OpenCodeRunEvent> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = value
                .pointer("/part/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(OpenCodeRunEvent::Text { text })
        }
        Some("tool_use") => {
            let command = value
                .pointer("/part/state/input/command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let output = value
                .pointer("/part/state/output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let exit_code = match value.pointer("/part/state/status").and_then(Value::as_str) {
                Some("completed") => Some(0),
                Some("error") => Some(1),
                _ => None,
            };
            Some(OpenCodeRunEvent::ToolUse {
                command,
                exit_code,
                output,
            })
        }
        Some("step_finish") => {
            let reason = value
                .pointer("/part/reason")
                .and_then(Value::as_str)
                .unwrap_or("stop")
                .to_string();
            let tokens = parse_step_finish_tokens(&value);
            Some(OpenCodeRunEvent::StepFinish { reason, tokens })
        }
        _ => None,
    }
}

fn parse_step_finish_tokens(value: &Value) -> TokenUsage {
    let input = value
        .pointer("/part/tokens/input")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .pointer("/part/tokens/output")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = value
        .pointer("/part/tokens/total")
        .and_then(Value::as_u64)
        .unwrap_or(input.saturating_add(output));
    let cost = value
        .pointer("/part/cost")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: total,
        cost_usd: cost,
    }
}

/// Build the permission env value for an explicit allowlist.
///
/// An explicitly empty list means deny-all, mirroring
/// `AgentRequest::allowed_tools` semantics. Full mode bypasses this helper.
fn permission_env_value(tools: &[String]) -> String {
    if tools.is_empty() {
        return r#"{"*":"deny"}"#.to_string();
    }
    let mut map = serde_json::Map::new();
    for tool in tools {
        map.insert(tool.clone(), Value::String("allow".to_string()));
    }
    serde_json::Value::Object(map).to_string()
}

pub struct OpenCodeAgent {
    pub cli_path: PathBuf,
    pub default_model: String,
    pub sandbox_mode: SandboxMode,
    /// Maximum seconds of idle silence on the output stream before the
    /// subprocess is declared a zombie and terminated. `None` = no timeout.
    pub stream_timeout_secs: Option<u64>,
}

impl OpenCodeAgent {
    pub fn from_config(config: OpenCodeAgentConfig, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path: config.cli_path,
            default_model: config.default_model,
            sandbox_mode,
            stream_timeout_secs: None,
        }
    }

    pub fn with_stream_timeout(mut self, stream_timeout_secs: Option<u64>) -> Self {
        self.stream_timeout_secs = stream_timeout_secs;
        self
    }

    fn effective_model(&self, req: &AgentRequest) -> Option<String> {
        req.model
            .clone()
            .or_else(|| (!self.default_model.is_empty()).then(|| self.default_model.clone()))
    }

    fn base_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("run"),
            OsString::from("--format"),
            OsString::from("json"),
        ];
        if let Some(model) = self.effective_model(req) {
            args.push(OsString::from("--model"));
            args.push(OsString::from(model));
        }
        if req.uses_dangerously_skip_permissions() {
            args.push(OsString::from("--auto"));
        }
        args.push(OsString::from(req.prompt.clone()));
        args
    }

    fn spawn_env_vars(&self, req: &AgentRequest) -> Vec<(String, String)> {
        let mut vars = Vec::new();
        if !req.uses_dangerously_skip_permissions() {
            let tools = req.scoped_allowed_tools();
            vars.push((
                OPENCODE_PERMISSION_ENV.to_string(),
                permission_env_value(&tools),
            ));
        }
        vars
    }

    fn item_from_tool_use(&self, event: &OpenCodeRunEvent) -> Option<Item> {
        match event {
            OpenCodeRunEvent::ToolUse {
                command,
                exit_code,
                output,
            } => Some(Item::ShellCommand {
                command: command.clone(),
                exit_code: *exit_code,
                stdout: output.clone(),
                stderr: String::new(),
            }),
            _ => None,
        }
    }
}

#[async_trait]
impl CodeAgent for OpenCodeAgent {
    fn name(&self) -> &str {
        "opencode"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        let base_args = self.base_args(&req);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(self.sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(self.sandbox_mode, &req.project_root)
        };
        let mut spawn_env_vars = req.env_vars.clone();
        for (key, value) in self.spawn_env_vars(&req) {
            spawn_env_vars.insert(key, value);
        }
        let run_identity = crate::resolve_agent_run_identity(&spawn_env_vars);
        run_identity.write_env_vars(&mut spawn_env_vars);
        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.cli_path,
                args: &base_args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox_spec,
                env_vars: &spawn_env_vars,
                permission_mode: req.permission_mode,
                forward_stdin: false,
            })?;

        let spawn_error_req = req.clone();
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "opencode",
                process_label: "opencode run",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(Stdio::null()),
                extra_env_removals: Vec::new(),
                map_spawn_error: Box::new(move |error, spawn| {
                    let message = crate::classify_missing_workspace_spawn_failure(
                        error,
                        &spawn_error_req.project_root,
                        format!(
                            "failed to spawn opencode run: {error}; program={}; current_dir={}; sandbox_engine={:?}",
                            spawn.program.display(),
                            spawn.current_dir.display(),
                            spawn.sandbox_engine,
                        ),
                    );
                    harness_core::error::HarnessError::AgentExecution(message)
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let mut child = supervised.child;
        let limits = crate::OutputLimits::from_stream_timeout_secs(self.stream_timeout_secs);
        let output = child.wait_with_output(&limits).await.map_err(|error| {
            harness_core::error::HarnessError::AgentExecution(format!(
                "failed to wait for opencode: {error}"
            ))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        log_captured_stderr_diagnostics(&stderr, self.name());

        if !output.status.success() {
            let error_output = if stderr.trim().is_empty() {
                stdout.clone()
            } else {
                stderr.clone()
            };
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "opencode exited with {status}: {error_output}",
                status = output.status
            )));
        }

        let mut items = Vec::new();
        let mut message_text = String::new();
        let mut token_usage = TokenUsage::default();
        for line in stdout.lines() {
            match parse_opencode_run_line(line) {
                Some(OpenCodeRunEvent::Text { text }) => message_text.push_str(&text),
                Some(event @ OpenCodeRunEvent::ToolUse { .. }) => {
                    if let Some(item) = self.item_from_tool_use(&event) {
                        items.push(item);
                    }
                }
                Some(OpenCodeRunEvent::StepFinish { reason, tokens }) => {
                    token_usage = tokens;
                    if reason != "stop" {
                        return Err(harness_core::error::HarnessError::AgentExecution(format!(
                            "opencode run finished with reason `{reason}`"
                        )));
                    }
                }
                None => {}
            }
        }

        Ok(AgentResponse {
            output: message_text,
            stderr,
            items,
            token_usage,
            model: "opencode".to_string(),
            exit_code: output.status.code(),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        if let Some(ref token) = req.capability_token {
            if token.is_expired() {
                return Err(harness_core::error::HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }

        let base_args = self.base_args(&req);
        let sandbox_spec = if let Some(ref token) = req.capability_token {
            SandboxSpec::new(self.sandbox_mode, &req.project_root)
                .with_allowed_write_paths(token.allowed_write_paths.clone())
        } else {
            SandboxSpec::new(self.sandbox_mode, &req.project_root)
        };
        let mut spawn_env_vars = req.env_vars.clone();
        for (key, value) in self.spawn_env_vars(&req) {
            spawn_env_vars.insert(key, value);
        }
        let run_identity = crate::resolve_agent_run_identity(&spawn_env_vars);
        run_identity.write_env_vars(&mut spawn_env_vars);
        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.cli_path,
                args: &base_args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox_spec,
                env_vars: &spawn_env_vars,
                permission_mode: req.permission_mode,
                forward_stdin: false,
            })?;

        let spawn_error_req = req.clone();
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity,
                native_kind: "opencode",
                process_label: "opencode run (streamed)",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(Stdio::null()),
                extra_env_removals: Vec::new(),
                map_spawn_error: Box::new(move |error, spawn| {
                    let message = crate::classify_missing_workspace_spawn_failure(
                        error,
                        &spawn_error_req.project_root,
                        format!(
                            "failed to spawn opencode run: {error}; program={}; current_dir={}; sandbox_engine={:?}",
                            spawn.program.display(),
                            spawn.current_dir.display(),
                            spawn.sandbox_engine,
                        ),
                    );
                    harness_core::error::HarnessError::AgentExecution(message)
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let mut child = supervised.child;

        let stderr_capture = Arc::new(Mutex::new(String::new()));
        let mut stderr_task = None;
        if let Some(stderr) = child.inner_mut().stderr.take() {
            let agent = self.name().to_string();
            let captured = Arc::clone(&stderr_capture);
            stderr_task = Some(tokio::spawn(async move {
                capture_agent_stderr_diagnostics(stderr, &agent, Some(captured)).await;
            }));
        }

        let stdout = child.inner_mut().stdout.take().ok_or_else(|| {
            harness_core::error::HarnessError::AgentExecution("opencode stdout unavailable".into())
        })?;
        let mut lines: Lines<BufReader<ChildStdout>> = BufReader::new(stdout).lines();
        let idle_timeout = self
            .stream_timeout_secs
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs);

        let mut stream_result: harness_core::error::Result<()> = Ok(());
        loop {
            let read = match idle_timeout {
                Some(duration) => match tokio::time::timeout(duration, lines.next_line()).await {
                    Ok(result) => result,
                    Err(_) => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "opencode stream idle timeout",
                    )),
                },
                None => lines.next_line().await,
            };
            let line = match read {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                        format!("failed reading opencode stdout: {error}"),
                    ));
                    break;
                }
            };
            let Some(event) = parse_opencode_run_line(&line) else {
                continue;
            };
            match event {
                OpenCodeRunEvent::Text { text } => {
                    if send_stream_item(&tx, StreamItem::MessageDelta { text }, self.name(), "text")
                        .await
                        .is_err()
                    {
                        stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode stream send failed".into(),
                        ));
                        break;
                    }
                }
                OpenCodeRunEvent::ToolUse {
                    command,
                    exit_code,
                    output,
                } => {
                    let item = Item::ShellCommand {
                        command: command.clone(),
                        exit_code,
                        stdout: output.clone(),
                        stderr: String::new(),
                    };
                    if send_stream_item(
                        &tx,
                        StreamItem::ItemStarted { item: item.clone() },
                        self.name(),
                        "tool",
                    )
                    .await
                    .is_err()
                    {
                        stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode stream send failed".into(),
                        ));
                        break;
                    }
                    if send_stream_item(
                        &tx,
                        StreamItem::ToolOutputDelta {
                            item_id: command,
                            text: output,
                        },
                        self.name(),
                        "tool output",
                    )
                    .await
                    .is_err()
                    {
                        stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode stream send failed".into(),
                        ));
                        break;
                    }
                    if send_stream_item(
                        &tx,
                        StreamItem::ItemCompleted { item },
                        self.name(),
                        "tool completed",
                    )
                    .await
                    .is_err()
                    {
                        stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode stream send failed".into(),
                        ));
                        break;
                    }
                }
                OpenCodeRunEvent::StepFinish { reason, tokens } => {
                    if send_stream_item(
                        &tx,
                        StreamItem::TokenUsage { usage: tokens },
                        self.name(),
                        "usage",
                    )
                    .await
                    .is_err()
                    {
                        stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                            "opencode stream send failed".into(),
                        ));
                        break;
                    }
                    if reason != "stop" {
                        stream_result = Err(harness_core::error::HarnessError::AgentExecution(
                            format!("opencode run finished with reason `{reason}`"),
                        ));
                    }
                    break;
                }
            }
        }

        let status = child
            .wait_and_cleanup_descendants()
            .await
            .map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "failed waiting for opencode process: {error}"
                ))
            })?;
        if let Some(stderr_task) = stderr_task {
            let _ = stderr_task.await;
        }
        if let Err(error) = stream_result {
            let stderr = captured_stderr_tail(&stderr_capture);
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "{error}; stderr=[{stderr}]"
            )));
        }
        if !status.success() {
            let stderr = captured_stderr_tail(&stderr_capture);
            return Err(harness_core::error::HarnessError::AgentExecution(format!(
                "opencode exited with {status}: {stderr}"
            )));
        }
        send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_line() {
        let line = r#"{"type":"text","timestamp":1,"part":{"type":"text","text":"OK"}}"#;
        assert_eq!(
            parse_opencode_run_line(line),
            Some(OpenCodeRunEvent::Text {
                text: "OK".to_string()
            })
        );
    }

    #[test]
    fn parses_tool_use_line() {
        let line = r#"{"type":"tool_use","part":{"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls /tmp"},"output":"a.txt"}}}"#;
        assert_eq!(
            parse_opencode_run_line(line),
            Some(OpenCodeRunEvent::ToolUse {
                command: "ls /tmp".to_string(),
                exit_code: Some(0),
                output: "a.txt".to_string()
            })
        );

        let failed = r#"{"type":"tool_use","part":{"type":"tool","tool":"bash","callID":"c2","state":{"status":"error","input":{"command":"ls /nope"},"output":""}}}"#;
        assert_eq!(
            parse_opencode_run_line(failed),
            Some(OpenCodeRunEvent::ToolUse {
                command: "ls /nope".to_string(),
                exit_code: Some(1),
                output: String::new()
            })
        );
    }

    #[test]
    fn parses_step_finish_tokens() {
        let line = r#"{"type":"step_finish","part":{"reason":"stop","tokens":{"total":100,"input":90,"output":10,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0.001}}"#;
        let event = parse_opencode_run_line(line).unwrap();
        match event {
            OpenCodeRunEvent::StepFinish { reason, tokens } => {
                assert_eq!(reason, "stop");
                assert_eq!(tokens.input_tokens, 90);
                assert_eq!(tokens.output_tokens, 10);
                assert_eq!(tokens.total_tokens, 100);
                assert!((tokens.cost_usd - 0.001).abs() < f64::EPSILON);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn ignores_unknown_lines() {
        assert!(parse_opencode_run_line("not json").is_none());
        assert!(parse_opencode_run_line(r#"{"type":"step_start","part":{}}"#).is_none());
        assert!(parse_opencode_run_line("").is_none());
    }

    #[test]
    fn permission_env_mapping() {
        assert_eq!(
            permission_env_value(&["bash".to_string(), "edit".to_string()]),
            r#"{"bash":"allow","edit":"allow"}"#
        );
        assert_eq!(permission_env_value(&[]), r#"{"*":"deny"}"#);
    }
}
