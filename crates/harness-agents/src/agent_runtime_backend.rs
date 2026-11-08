//! Opt-in backend that drives Claude, Codex and OpenCode through the
//! agent-runtime library instead of the native adapters in this crate.
//!
//! It is registered only for the agent names listed in
//! `[agents.runtime_backend].enabled_for`; every other agent keeps its native
//! adapter, so switching is a config change and rolling back is removing the
//! list. Each call appends one line to the trial log (see `trial.rs`).
//!
//! Rules kept on purpose, because they are what the trial measures:
//! - success is only `TurnOutcome::Completed`; `Unknown`, `Failed` and
//!   `Cancelled` are errors, so a missing login or exhausted quota can never be
//!   reported as success;
//! - approvals are answered once, never "always", and only for tools the
//!   request's allowlist grants;
//! - the session is always closed and its leftover report is recorded.

#[path = "agent_runtime_backend/approval.rs"]
mod approval;
#[path = "agent_runtime_backend/trial.rs"]
mod trial;

use agent_runtime::core::{FailureClass, TurnOutcome};
use agent_runtime::{Delivery, Event, OpenRequest, Registry, Runtime, ThreadSpec};
use async_trait::async_trait;
use harness_core::agent::{
    AgentBackend, AgentRequest, AgentResponse, ApprovalDecision, StreamItem,
};
use harness_core::config::agents::{AgentRuntimeBackendConfig, SandboxMode};
use harness_core::error::HarnessError;
use harness_core::types::{Capability, Item, TokenUsage};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use trial::{default_trial_log, TrialRecord};

/// One harness agent name served by agent-runtime.
pub struct AgentRuntimeBackend {
    name: String,
    agent: String,
    via: Option<String>,
    mode: Option<String>,
    sandbox_mode: SandboxMode,
    runtime: Arc<Runtime>,
    trial_log: std::path::PathBuf,
}

/// Builds the shared agent-runtime `Runtime` from its default config
/// directory plus the configured override files.
pub fn runtime_from_config(config: &AgentRuntimeBackendConfig) -> anyhow::Result<Arc<Runtime>> {
    let registry = Registry::with_overrides(&config.config_files)
        .map_err(|error| anyhow::anyhow!("agent-runtime registry: {error}"))?;
    Ok(Arc::new(Runtime::new(registry)))
}

impl AgentRuntimeBackend {
    pub fn new(
        name: &str,
        config: &AgentRuntimeBackendConfig,
        sandbox_mode: SandboxMode,
        runtime: Arc<Runtime>,
    ) -> Self {
        Self {
            name: name.to_string(),
            agent: config
                .agent_ids
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string()),
            via: config.channels.get(name).cloned(),
            mode: config.modes.get(name).cloned(),
            sandbox_mode,
            runtime,
            trial_log: config.trial_log.clone().unwrap_or_else(default_trial_log),
        }
    }

    /// Codex takes `<approval>/<sandbox>`; the harness sandbox decides the
    /// second half. Other agents keep their profile's default mode.
    fn mode_for(&self) -> Option<String> {
        if self.mode.is_some() {
            return self.mode.clone();
        }
        if self.agent != "codex" {
            return None;
        }
        let sandbox = match self.sandbox_mode {
            SandboxMode::ReadOnly | SandboxMode::ReadOnlyWithNetwork => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        };
        Some(format!("on-request/{sandbox}"))
    }

    fn open_request(&self, req: &AgentRequest) -> OpenRequest {
        OpenRequest {
            agent: self.agent.clone(),
            via: self.via.clone(),
            cwd: req.project_root.clone(),
            mode: self.mode_for(),
            model: req.model.clone(),
            effort: req.reasoning_effort.clone(),
            env: req
                .env_vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            ..OpenRequest::default()
        }
    }

    /// Runs one turn. Every event is forwarded to `tx` when given. Returns the
    /// collected output only for a `Completed` turn.
    async fn run(
        &self,
        req: AgentRequest,
        tx: Option<&tokio::sync::mpsc::Sender<StreamItem>>,
    ) -> harness_core::error::Result<(String, TokenUsage)> {
        let started = Instant::now();
        let open = self.open_request(&req);
        let channel = self
            .runtime
            .select_channel(&open.agent, open.via.as_deref())
            .map(|s| s.channel)
            .ok();
        let mut record = TrialRecord::start(
            &self.name,
            &self.agent,
            channel.clone(),
            open.mode.clone(),
            open.model.clone(),
        );
        let result = self.run_inner(&req, open, tx, &mut record).await;
        record.finish(started.elapsed(), &result);
        record.append(&self.trial_log);
        result
    }

    async fn run_inner(
        &self,
        req: &AgentRequest,
        open: OpenRequest,
        tx: Option<&tokio::sync::mpsc::Sender<StreamItem>>,
        record: &mut TrialRecord,
    ) -> harness_core::error::Result<(String, TokenUsage)> {
        if let Some(token) = &req.capability_token {
            if token.is_expired() {
                return Err(HarnessError::AgentExecution(format!(
                    "capability token for subtask {} has expired",
                    token.subtask_index
                )));
            }
        }
        let mut session = self.runtime.open(&open).await.map_err(|error| {
            HarnessError::AgentExecution(format!("agent-runtime {}: {error}", self.agent))
        })?;
        let mut events = session
            .take_events()
            .ok_or_else(|| HarnessError::AgentExecution("agent-runtime: no event stream".into()))?;
        let outcome = async {
            let thread = session.start(ThreadSpec::new(&req.project_root)).await?;
            let turn = session
                .submit(&thread, &req.prompt, Delivery::NewTurn)
                .await?;
            if let Some(tx) = tx {
                let _ = tx.send(StreamItem::TurnStarted).await;
            }
            let grants = approval::Grants::from_request(req);
            let mut output = String::new();
            let mut usage = TokenUsage::default();
            let deadline = req
                .timeout_secs
                .map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
            loop {
                let next = match deadline {
                    Some(at) => match tokio::time::timeout_at(at, events.recv()).await {
                        Ok(next) => next,
                        Err(_) => {
                            let _ = session.interrupt(&thread).await;
                            return Err(agent_runtime::OpError::Timeout(format!(
                                "turn exceeded {:?}",
                                req.timeout_secs
                            )));
                        }
                    },
                    None => events.recv().await,
                };
                let Some(envelope) = next else {
                    return Ok((
                        TurnOutcome::Unknown {
                            reason: "event stream ended".into(),
                        },
                        output,
                        usage,
                    ));
                };
                if envelope.turn.as_ref().is_some_and(|t| *t != turn) {
                    continue;
                }
                match envelope.event {
                    Event::MessageChunk {
                        role: agent_runtime::core::Role::Assistant,
                        text,
                        ..
                    } => {
                        output.push_str(&text);
                        if let Some(tx) = tx {
                            let _ = tx.send(StreamItem::MessageDelta { text }).await;
                        }
                    }
                    Event::Tool {
                        title, raw, status, ..
                    } => {
                        if let (Some(tx), Some(name)) = (tx, title) {
                            if status.as_deref() != Some("completed") {
                                let _ = tx.send(StreamItem::ToolCall { name, input: raw }).await;
                            }
                        }
                    }
                    Event::Usage(report) => {
                        usage.input_tokens = report.input_tokens.unwrap_or(usage.input_tokens);
                        usage.output_tokens = report.output_tokens.unwrap_or(usage.output_tokens);
                        usage.total_tokens = usage.input_tokens + usage.output_tokens;
                        let observed = report.cost.as_ref().and_then(serde_json::Value::as_f64);
                        if let Some(cost) = observed {
                            usage.cost_usd = cost;
                        }
                        record.usage(usage.input_tokens, usage.output_tokens, observed);
                        if let Some(tx) = tx {
                            let _ = tx
                                .send(StreamItem::TokenUsage {
                                    usage: usage.clone(),
                                    cost_usd_observed: observed.is_some(),
                                })
                                .await;
                        }
                    }
                    Event::Request(request) => {
                        let reply = grants.decide(&request);
                        record.approval(reply.is_some());
                        if let Some(tx) = tx {
                            let _ = tx
                                .send(StreamItem::ApprovalRequest {
                                    id: request.id.0.clone(),
                                    command: request.title.clone().unwrap_or_default(),
                                })
                                .await;
                        }
                        let reply = reply.unwrap_or_else(|| approval::reject(&request));
                        session.respond(&request.id, reply).await?;
                    }
                    Event::Notice { title, detail, .. } => {
                        if let Some(tx) = tx {
                            let message = format!(
                                "{title}{}",
                                detail.map(|d| format!(": {d}")).unwrap_or_default()
                            );
                            let _ = tx.send(StreamItem::Warning { message }).await;
                        }
                    }
                    Event::TurnEnded(end) => {
                        record.outcome(&end);
                        return Ok((end.outcome, output, usage));
                    }
                    _ => {}
                }
            }
        }
        .await;
        // Always close and record what the close found, success or not.
        match session.close().await {
            Ok(report) => record.closed(&report),
            Err(error) => record.close_error(&error.to_string()),
        }
        while events.recv().await.is_some() {}
        let (outcome, output, usage) = outcome.map_err(|error| {
            HarnessError::AgentExecution(format!("agent-runtime {}: {error}", self.agent))
        })?;
        match outcome {
            TurnOutcome::Completed => Ok((output, usage)),
            TurnOutcome::Cancelled => Err(HarnessError::AgentExecution(format!(
                "agent-runtime {}: turn cancelled",
                self.agent
            ))),
            TurnOutcome::Failed { failure } => Err(HarnessError::AgentExecution(format!(
                "agent-runtime {}: turn failed ({})",
                self.agent,
                describe(&failure)
            ))),
            other => Err(HarnessError::AgentExecution(format!(
                "agent-runtime {}: turn did not complete: {other:?}",
                self.agent
            ))),
        }
    }
}

fn describe(failure: &FailureClass) -> String {
    match failure {
        FailureClass::UsageLimit { detail } => format!("usage limit: {detail}"),
        FailureClass::Auth { detail } => format!("auth: {detail}"),
        FailureClass::Network { detail } => format!("network: {detail}"),
        FailureClass::ProcessExit { code, signal } => {
            format!("process exit code={code:?} signal={signal:?}")
        }
        FailureClass::Stalled { idle_ms, detail } => {
            format!("stalled: no event for {}s ({detail})", idle_ms / 1000)
        }
        FailureClass::Other { detail } => detail.clone(),
    }
}

#[async_trait]
impl AgentBackend for AgentRuntimeBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> String {
        format!("agent-runtime:{}", self.agent)
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        let model = req.model.clone().unwrap_or_default();
        let (output, token_usage) = self.run(req, None).await?;
        Ok(AgentResponse {
            output,
            stderr: String::new(),
            items: Vec::new(),
            token_usage,
            model,
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        let (output, _) = self.run(req, Some(&tx)).await?;
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning { content: output },
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }

    /// Approvals are answered inside the turn from the request's allowlist;
    /// there is no separate live channel to answer from outside.
    async fn respond_approval(
        &self,
        _id: String,
        _decision: ApprovalDecision,
    ) -> harness_core::error::Result<()> {
        Err(HarnessError::Unsupported(
            "agent-runtime backend answers approvals from the allowlist inside the turn".into(),
        ))
    }
}

#[cfg(test)]
#[path = "agent_runtime_backend/tests.rs"]
mod tests;
