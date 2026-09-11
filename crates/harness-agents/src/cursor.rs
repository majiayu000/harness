//! Cursor Agent's non-interactive CLI surface (`cursor-agent --print`).
use async_trait::async_trait;
use harness_core::agent::{
    AgentBackend, AgentEvent, AgentRequest, AgentResponse, ModelIdentitySource,
};
use harness_core::config::agents::{AgentPermissionMode, CursorAgentConfig, SandboxMode};
use harness_core::error::{HarnessError, Result};
use harness_core::types::{Capability, Item, TokenUsage};
use harness_sandbox::SandboxSpec;
use serde_json::Value;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;

pub struct CursorAgent {
    config: CursorAgentConfig,
    sandbox_mode: SandboxMode,
    stream_timeout_secs: Option<u64>,
}

fn failure(message: impl Into<String>) -> HarnessError {
    HarnessError::AgentExecution(message.into())
}

impl CursorAgent {
    pub fn from_config(config: CursorAgentConfig, sandbox_mode: SandboxMode) -> Self {
        Self {
            config,
            sandbox_mode,
            stream_timeout_secs: None,
        }
    }

    pub fn with_stream_timeout(mut self, seconds: Option<u64>) -> Self {
        self.stream_timeout_secs = seconds;
        self
    }

    fn args(&self, req: &AgentRequest) -> Result<Vec<OsString>> {
        if req.permission_mode != AgentPermissionMode::Full || req.allowed_tools.is_some() {
            return Err(HarnessError::Unsupported(
                "Cursor Agent CLI requires capability_profile = \"full\" and no allowed_tools override; scoped tool allowlists are not supported".into(),
            ));
        }
        if req.approval_policy.is_some() || req.reasoning_effort.is_some() {
            return Err(HarnessError::Unsupported(
                "Cursor Agent CLI does not map approval_policy or reasoning_effort; select a Cursor model directly".into(),
            ));
        }
        if req.max_budget_usd.is_some() {
            return Err(HarnessError::Unsupported(
                "Cursor Agent CLI does not report enforceable USD usage".into(),
            ));
        }
        let mut args: Vec<OsString> = [
            "--print",
            "--force",
            "--trust",
            "--output-format",
            "stream-json",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        let model = req.model.as_deref().unwrap_or(&self.config.default_model);
        if !model.is_empty() {
            args.extend([OsString::from("--model"), OsString::from(model)]);
        }
        // Terminate option parsing so a prompt beginning with '-' stays data.
        args.extend([OsString::from("--"), OsString::from(&req.prompt)]);
        Ok(args)
    }

    async fn run(
        &self,
        req: AgentRequest,
        tx: Option<&Sender<AgentEvent>>,
    ) -> Result<AgentResponse> {
        let args = self.args(&req)?;
        let mut sandbox = SandboxSpec::new(
            req.sandbox_mode.unwrap_or(self.sandbox_mode),
            &req.project_root,
        );
        if let Some(token) = &req.capability_token {
            sandbox = sandbox.with_allowed_write_paths(token.allowed_write_paths.clone());
        }
        let mut env_vars = req.env_vars.clone();
        let identity = crate::resolve_agent_run_identity(&env_vars);
        identity.write_env_vars(&mut env_vars);
        let prepared_spawn =
            crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
                program: &self.config.cli_path,
                args: &args,
                project_root: &req.project_root,
                sandbox_spec: &sandbox,
                env_vars: &env_vars,
                secret_env_keys: &["CURSOR_API_KEY".into(), "CURSOR_AUTH_TOKEN".into()],
                container_bind_mounts: &[],
                permission_mode: req.permission_mode,
                forward_stdin: false,
            })
            .await?;
        let supervised = crate::spawn_supervisor::spawn_agent(
            crate::spawn_supervisor::AgentSpawnPlan {
                prepared_spawn,
                run_identity: identity,
                native_kind: "cursor",
                process_label: "cursor-agent --print",
                stdio: crate::spawn_supervisor::AgentStdio::piped_output(Stdio::null()),
                extra_env_removals: Vec::new(),
                map_spawn_error: Box::new(|error, spawn| {
                    failure(crate::classify_missing_workspace_spawn_failure(
                        error,
                        &spawn.current_dir,
                        format!(
                            "failed to spawn Cursor Agent: {error}; program={}",
                            spawn.program.display()
                        ),
                    ))
                }),
            },
            req.capability_token.as_ref(),
        )
        .await?;
        let mut child = supervised.child;
        let capture = Arc::new(Mutex::new(String::new()));
        let stderr = child
            .inner_mut()
            .stderr
            .take()
            .ok_or_else(|| failure("Cursor stderr unavailable"))?;
        let captured = capture.clone();
        // Keep both readers in this future: cancellation drops them with the managed child.
        let read_stderr =
            crate::streaming::capture_agent_stderr_diagnostics(stderr, "cursor", Some(captured));
        let stdout = child
            .inner_mut()
            .stdout
            .take()
            .ok_or_else(|| failure("Cursor stdout unavailable"))?;
        let mut lines = BufReader::new(stdout).lines();
        let idle = self
            .stream_timeout_secs
            .filter(|s| *s > 0)
            .map(Duration::from_secs);
        let mut state = CursorOutput::default();
        let mut canary_verified = !child.awaits_container_egress_canary();
        let preverified = child.egress_verified_before_spawn();
        let execution = async {
            let result = {
                let read_stdout = async {
                    if preverified {
                        emit(tx, AgentEvent::EgressVerifiedAtDispatch).await?;
                    }
                    loop {
                        let read = async {
                            match idle {
                                Some(timeout) => tokio::time::timeout(timeout, lines.next_line())
                                    .await
                                    .map_err(|_| failure("Cursor Agent stream idle timeout"))?,
                                None => lines.next_line().await,
                            }
                            .map_err(|error| {
                                failure(format!("failed reading Cursor Agent output: {error}"))
                            })
                        };
                        let line = tokio::select! {
                            _ = async { if let Some(tx) = tx { tx.closed().await } else { std::future::pending::<()>().await } } => {
                                return Err(failure("Cursor Agent output receiver closed"));
                            }
                            line = read => line?,
                        };
                        let Some(line) = line else {
                            break;
                        };
                        if line == crate::spawn_contract::egress::CONTAINER_EGRESS_CANARY_VERIFIED {
                            if !canary_verified {
                                canary_verified = true;
                                emit(tx, AgentEvent::EgressVerifiedAtDispatch).await?;
                            }
                            continue;
                        }
                        for event in state.parse(&line)? {
                            emit(tx, event).await?;
                        }
                    }
                    if !canary_verified {
                        return Err(failure(
                            "Cursor exited before the container egress canary succeeded",
                        ));
                    }
                    if state.output.is_none() {
                        return Err(failure(
                            "Cursor Agent exited without a successful terminal result",
                        ));
                    }
                    let status = child
                        .wait_and_cleanup_descendants()
                        .await
                        .map_err(|error| {
                            failure(format!("failed waiting for Cursor Agent: {error}"))
                        })?;
                    if !status.success() {
                        return Err(failure(format!("Cursor Agent exited with {status}")));
                    }
                    Ok(status.code())
                };
                tokio::select! {
                    result = read_stdout => result,
                    _ = async {
                        match req.timeout_secs.filter(|s| *s > 0) {
                            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => Err(failure("Cursor Agent turn timeout")),
                }
            };
            // Terminate on read/parse/timeout errors before awaiting stderr EOF.
            if result.is_err() {
                child.terminate_now();
            }
            result
        };
        let (result, ()) = tokio::join!(execution, read_stderr);
        let stderr = crate::streaming::captured_stderr_tail(&capture);
        let exit_code = result.map_err(|error| failure(format!("{error}; stderr=[{stderr}]")))?;
        let output = state
            .output
            .ok_or_else(|| failure("Cursor terminal result missing"))?;
        emit(
            tx,
            AgentEvent::TurnCompleted {
                output: output.clone(),
            },
        )
        .await?;
        emit(tx, AgentEvent::Done).await?;
        Ok(AgentResponse {
            output,
            stderr,
            items: state.items,
            token_usage: TokenUsage::default(),
            model: state.model.unwrap_or_else(|| {
                req.model
                    .unwrap_or_else(|| self.config.default_model.clone())
            }),
            exit_code,
        })
    }
}

async fn emit(tx: Option<&Sender<AgentEvent>>, event: AgentEvent) -> Result<()> {
    if let Some(tx) = tx {
        tx.send(event)
            .await
            .map_err(|_| failure("Cursor Agent output receiver closed"))?;
    }
    Ok(())
}

#[derive(Default)]
struct CursorOutput {
    model: Option<String>,
    items: Vec<Item>,
    output: Option<String>,
}

impl CursorOutput {
    fn parse(&mut self, line: &str) -> Result<Vec<AgentEvent>> {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| failure(format!("invalid Cursor Agent JSON event: {error}")))?;
        let kind = value["type"]
            .as_str()
            .ok_or_else(|| failure("Cursor event has no type"))?;
        let mut events = Vec::new();
        match kind {
            "system" if value["subtype"] == "init" => {
                events.push(AgentEvent::TurnStarted);
                if let Some(model) = value["model"].as_str() {
                    self.model = Some(model.to_string());
                    events.push(AgentEvent::ModelReported {
                        model: model.into(),
                        source: ModelIdentitySource::ProviderReported,
                    });
                }
            }
            "assistant" => {
                let content = value
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .ok_or_else(|| failure("Cursor assistant event has no message content"))?;
                for block in content {
                    if block["type"] == "text" {
                        let text = block["text"]
                            .as_str()
                            .ok_or_else(|| failure("Cursor text block has no text"))?;
                        let item = Item::AgentReasoning {
                            content: text.into(),
                        };
                        self.items.push(item.clone());
                        events.push(AgentEvent::ItemCompleted { item });
                    }
                }
            }
            "tool_call" => {
                let calls = value["tool_call"]
                    .as_object()
                    .ok_or_else(|| failure("Cursor tool event has no tool_call"))?;
                // The envelope also contains timestamps, IDs and hook metadata.
                // Only tagged tool payloads are calls.
                let tool_calls: Vec<_> = calls
                    .iter()
                    .filter(|(name, _)| name.ends_with("ToolCall"))
                    .collect();
                if tool_calls.is_empty() {
                    return Err(failure("Cursor tool event contains no tool payload"));
                }
                if !matches!(value["subtype"].as_str(), Some("started" | "completed")) {
                    return Err(failure("Cursor tool event has an unsupported subtype"));
                }
                for (name, call) in tool_calls {
                    if !call.is_object() {
                        return Err(failure("Cursor tool payload must be an object"));
                    }
                    if value["subtype"] == "started" {
                        events.push(AgentEvent::ToolCall {
                            name: name.clone(),
                            input: call.clone(),
                        });
                    } else {
                        let item = Item::ToolCall {
                            name: name.clone(),
                            input: call["args"].clone(),
                            output: call.get("result").cloned(),
                        };
                        self.items.push(item.clone());
                        events.push(AgentEvent::ItemCompleted { item });
                        events.push(AgentEvent::ToolOutputDelta {
                            item_id: value["call_id"]
                                .as_str()
                                .ok_or_else(|| failure("Cursor tool event has no call_id"))?
                                .into(),
                            text: call.to_string(),
                        });
                    }
                }
            }
            "result" => {
                if value["is_error"] != false || value["subtype"] != "success" {
                    return Err(failure(format!(
                        "Cursor Agent reported failure: {}",
                        value["result"]
                    )));
                }
                if self.output.is_some() {
                    return Err(failure("Cursor Agent emitted multiple terminal results"));
                }
                self.output = Some(
                    value["result"]
                        .as_str()
                        .ok_or_else(|| failure("Cursor terminal event has no result text"))?
                        .into(),
                );
            }
            "error" => return Err(failure(format!("Cursor Agent error: {value}"))),
            "user" | "system" => {}
            _ => events.push(AgentEvent::Warning {
                message: format!("Unrecognized Cursor Agent event: {kind}"),
            }),
        }
        Ok(events)
    }
}

#[async_trait]
impl AgentBackend for CursorAgent {
    fn name(&self) -> &str {
        "cursor"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }
    async fn execute(&self, req: AgentRequest) -> Result<AgentResponse> {
        self.run(req, None).await
    }
    async fn execute_stream(&self, req: AgentRequest, tx: Sender<AgentEvent>) -> Result<()> {
        self.run(req, Some(&tx)).await.map(|_| ())
    }
}

#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
