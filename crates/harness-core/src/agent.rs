use crate::capability::CapabilityToken;
use crate::config::agents::{AgentPermissionMode, CapabilityProfile, SandboxMode};
use crate::config::HarnessConfig;
use crate::types::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;

pub const AGENT_ISOLATION_TIER_ENV: &str = "HARNESS_AGENT_ISOLATION_TIER";
pub const AGENT_NETWORK_ALLOWLIST_ENV: &str = "HARNESS_AGENT_NETWORK_ALLOWLIST";
pub const AGENT_OUTPUT_SCHEMA_PATH_ENV: &str = "HARNESS_AGENT_OUTPUT_SCHEMA_PATH";
pub const AGENT_CONTAINER_IMAGE_ENV: &str = "HARNESS_AGENT_CONTAINER_IMAGE";
pub const AGENT_EGRESS_PROXY_IMAGE_ENV: &str = "HARNESS_AGENT_EGRESS_PROXY_IMAGE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEgressMode {
    DenyAll,
    FirstPartyProxy,
    Unrestricted,
}

impl AgentEgressMode {
    pub fn resolve(permission_mode: AgentPermissionMode, allowlist: &[String]) -> Self {
        if !allowlist.is_empty() {
            Self::FirstPartyProxy
        } else if permission_mode == AgentPermissionMode::Full {
            Self::Unrestricted
        } else {
            Self::DenyAll
        }
    }
}

/// Core trait for all agent backends (Claude Code, Codex, Anthropic API, etc.).
///
/// Backends can be simple one-shot executors, streaming process supervisors, or
/// stateful protocol adapters. Control operations default to `Unsupported` so
/// one-shot executors only implement the execution surface they actually own.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn name(&self) -> &str;
    /// Stable identity used to detect "primary == challenger" misconfigurations
    /// in cross-review. Defaults to the registry key (`name`); implementations
    /// that wrap multiple backends should include the backend/model so two
    /// registry entries backed by the same model compare equal.
    fn id(&self) -> String {
        self.name().to_string()
    }
    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }
    async fn execute(&self, _req: AgentRequest) -> crate::error::Result<AgentResponse> {
        Err(crate::error::Error::Unsupported(
            "execute not supported".into(),
        ))
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> crate::error::Result<()> {
        self.start_turn(req, tx).await
    }

    /// Start a runtime turn. Stateful protocol backends override this; plain
    /// executors usually override `execute_stream` instead.
    async fn start_turn(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<AgentEvent>,
    ) -> crate::error::Result<()> {
        Err(crate::error::Error::Unsupported(
            "turn execution not supported".into(),
        ))
    }

    /// Interrupt an in-progress turn.
    async fn interrupt(&self) -> crate::error::Result<()> {
        Err(crate::error::Error::Unsupported(
            "interrupt not supported".into(),
        ))
    }

    /// Append instructions to an active turn (steer).
    async fn steer(&self, _text: String) -> crate::error::Result<()> {
        Err(crate::error::Error::Unsupported(
            "steer not supported".into(),
        ))
    }

    /// Respond to an approval request from the agent.
    async fn respond_approval(
        &self,
        _id: String,
        _decision: ApprovalDecision,
    ) -> crate::error::Result<()> {
        Err(crate::error::Error::Unsupported(
            "approval not supported".into(),
        ))
    }
}

pub use AgentBackend as AgentAdapter;
pub use AgentBackend as CodeAgent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    /// Canonical flattened prompt used for fallback execution and audit.
    pub prompt: String,
    /// Optional layered prompt payload for adapters with cache-friendly
    /// static prompt channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_layers: Option<AgentPromptLayers>,
    pub project_root: PathBuf,
    /// Requested permission mode. Legacy direct requests with no allowlist are
    /// also treated as unrestricted; see `effective_permission_mode`.
    #[serde(default)]
    pub permission_mode: AgentPermissionMode,
    /// Tool restriction for the agent invocation.
    ///
    /// - `None` → no restriction, preserving the legacy direct-request API.
    /// - `Some(tools)` → Restricted: CLI uses `--allowedTools <list>`.
    ///   An explicitly empty `Some(vec![])` means deny-all at the CLI boundary.
    ///
    /// This `Option` preserves the distinction between "no restriction configured"
    /// and "explicitly empty allowlist", preventing an emergency deny-all config
    /// (`allowed_tools = []`) from being silently promoted to full permissions.
    pub allowed_tools: Option<Vec<String>>,
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<SandboxMode>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub context: Vec<ContextItem>,
    /// Optional wall-clock turn timeout. One-shot direct calls may ignore this;
    /// protocol adapters use it for request/stream stall windows.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Execution phase for per-phase model selection via ReasoningBudget.
    /// When set and the agent has a ReasoningBudget configured, the phase
    /// determines which model is used. Defaults to None (uses req.model or default_model).
    #[serde(default)]
    pub execution_phase: Option<ExecutionPhase>,
    /// Additional environment variables to set in the agent subprocess.
    /// Used to pass per-task configuration such as `CARGO_TARGET_DIR` for
    /// workspace-isolated builds to prevent cargo lock contention.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Scoped write capability issued at dispatch time.
    ///
    /// When set, the agent checks expiry before spawning and the sandbox
    /// policy is narrowed to the token's `allowed_write_paths` instead of
    /// the blanket `project_root`. `None` means no token restriction.
    #[serde(skip)]
    pub capability_token: Option<CapabilityToken>,
}

impl AgentRequest {
    /// Apply operator-configured permissions and isolation to a direct agent
    /// request. Workflow-runtime requests resolve these fields per job and do
    /// not use this helper.
    pub fn apply_configured_policy(&mut self, config: &HarnessConfig) {
        self.permission_mode = config.agents.resolve_permission_mode();
        self.allowed_tools = config.agents.resolve_allowed_tools();
        self.env_vars.extend(configured_agent_spawn_env(config));
    }

    /// Returns `true` when the CLI should run without tool restrictions.
    ///
    /// When `true`, the CLI adapter should use `--dangerously-skip-permissions`.
    /// When `false`, the adapter should use `--allowedTools <list>` instead —
    /// these flags are mutually exclusive in Claude CLI 2.1.70+.
    pub fn uses_dangerously_skip_permissions(&self) -> bool {
        self.allowed_tools.is_none()
    }

    /// Resolves the permission mode used for egress. Explicit Full mode is
    /// independent from tool restrictions so correction turns can deny every
    /// tool without losing provider access. Legacy direct callers that set
    /// `allowed_tools` to `None` also retain unrestricted egress.
    pub fn effective_permission_mode(&self) -> AgentPermissionMode {
        if self.permission_mode == AgentPermissionMode::Full || self.allowed_tools.is_none() {
            AgentPermissionMode::Full
        } else {
            AgentPermissionMode::Scoped
        }
    }

    /// Resolve the tool list enforced by scoped backends. A missing list no
    /// longer means unrestricted access; it resolves to the Standard profile.
    pub fn scoped_allowed_tools(&self) -> Vec<String> {
        self.allowed_tools
            .clone()
            .unwrap_or_else(default_scoped_tools)
    }

    pub fn from_prompt_layers(prompt_layers: AgentPromptLayers, project_root: PathBuf) -> Self {
        Self {
            prompt: prompt_layers.to_prompt_string(),
            prompt_layers: Some(prompt_layers),
            project_root,
            ..Self::default()
        }
    }

    fn effective_prompt_layers(&self) -> Option<&AgentPromptLayers> {
        self.prompt_layers.as_ref()
    }

    pub fn claude_main_prompt(&self) -> Cow<'_, str> {
        self.effective_prompt_layers()
            .and_then(|layers| layers.main_prompt_for_cache())
            .map(Cow::Owned)
            .unwrap_or_else(|| Cow::Borrowed(self.prompt.as_str()))
    }

    pub fn claude_system_prompt(&self) -> Option<Cow<'_, str>> {
        self.effective_prompt_layers()
            .and_then(AgentPromptLayers::static_system_prompt_for_cache)
            .map(Cow::Borrowed)
    }
}

pub fn configured_agent_spawn_env(config: &HarnessConfig) -> HashMap<String, String> {
    let mut env_vars = HashMap::from([(
        AGENT_ISOLATION_TIER_ENV.to_string(),
        config.isolation.default_tier.as_str().to_string(),
    )]);
    let allowlist = config
        .isolation
        .network_allowlist
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    if !allowlist.is_empty() {
        env_vars.insert(AGENT_NETWORK_ALLOWLIST_ENV.to_string(), allowlist);
    }
    inherit_agent_spawn_control_env(&mut env_vars);
    env_vars
}

pub fn inherit_agent_spawn_control_env(env_vars: &mut HashMap<String, String>) {
    inherit_agent_spawn_control_env_with(env_vars, |key| {
        crate::config::process_env::non_blank_config_value(key)
    });
}

pub(crate) fn inherit_agent_spawn_control_env_with(
    env_vars: &mut HashMap<String, String>,
    mut read_process_env: impl FnMut(&str) -> Option<String>,
) {
    for key in [AGENT_CONTAINER_IMAGE_ENV, AGENT_EGRESS_PROXY_IMAGE_ENV] {
        if let Some(value) = read_process_env(key).filter(|value| !value.trim().is_empty()) {
            env_vars.insert(key.to_string(), value);
        }
    }
}

impl Default for AgentRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            prompt_layers: None,
            project_root: PathBuf::from("."),
            permission_mode: AgentPermissionMode::default(),
            allowed_tools: CapabilityProfile::default().tools(),
            model: None,
            reasoning_effort: None,
            sandbox_mode: None,
            approval_policy: None,
            max_budget_usd: None,
            context: Vec::new(),
            timeout_secs: None,
            execution_phase: None,
            env_vars: HashMap::new(),
            capability_token: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPromptLayers {
    /// Role, workflow, and output-format instructions that are stable for
    /// repeated tasks of the same prompt kind.
    pub static_instructions: String,
    /// Project and session context that should remain in the main prompt.
    pub context: String,
    /// Per-invocation task payload and appended runtime context.
    pub dynamic_payload: String,
}

impl AgentPromptLayers {
    pub fn new(
        static_instructions: impl Into<String>,
        context: impl Into<String>,
        dynamic_payload: impl Into<String>,
    ) -> Self {
        Self {
            static_instructions: static_instructions.into(),
            context: context.into(),
            dynamic_payload: dynamic_payload.into(),
        }
    }

    pub fn to_prompt_string(&self) -> String {
        let mut prompt = String::with_capacity(
            self.static_instructions.len() + self.context.len() + self.dynamic_payload.len(),
        );
        prompt.push_str(&self.static_instructions);
        prompt.push_str(&self.context);
        prompt.push_str(&self.dynamic_payload);
        prompt
    }

    pub fn append_to_dynamic_payload(&mut self, text: &str) {
        self.dynamic_payload.push_str(text);
    }

    pub fn static_system_prompt(&self) -> Option<&str> {
        if self.static_instructions.trim().is_empty() {
            None
        } else {
            Some(&self.static_instructions)
        }
    }

    pub fn static_system_prompt_for_cache(&self) -> Option<&str> {
        let has_main_prompt =
            !self.context.trim().is_empty() || !self.dynamic_payload.trim().is_empty();
        if has_main_prompt {
            self.static_system_prompt()
        } else {
            None
        }
    }

    pub fn main_prompt_for_cache(&self) -> Option<String> {
        self.static_system_prompt_for_cache()?;
        let mut prompt = String::with_capacity(self.context.len() + self.dynamic_payload.len());
        prompt.push_str(&self.context);
        prompt.push_str(&self.dynamic_payload);
        if prompt.trim().is_empty() {
            None
        } else {
            Some(prompt)
        }
    }
}

impl From<crate::prompts::PromptParts> for AgentPromptLayers {
    fn from(parts: crate::prompts::PromptParts) -> Self {
        Self::new(
            parts.static_instructions,
            parts.context,
            parts.dynamic_payload,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub output: String,
    pub stderr: String,
    pub items: Vec<Item>,
    pub token_usage: TokenUsage,
    pub model: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    EgressVerifiedAtDispatch,
    TurnStarted,
    ItemStarted {
        item: Item,
    },
    ItemStartedKind {
        item_type: String,
    },
    MessageDelta {
        text: String,
    },
    ToolOutputDelta {
        item_id: String,
        text: String,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ItemCompleted {
        item: Item,
    },
    ItemCompletedKind,
    TokenUsage {
        usage: TokenUsage,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
    TurnCompleted {
        output: String,
    },
    ApprovalRequest {
        id: String,
        command: String,
    },
    Done,
}

pub type StreamItem = AgentEvent;

/// Task classification for agent dispatch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskClassification {
    pub complexity: TaskComplexity,
    pub language: Option<Language>,
    pub requires_write: bool,
    pub requires_network: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskComplexity {
    Simple,
    Medium,
    Complex,
    Critical,
}

/// Decision for an approval request from the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Accept,
    Reject { reason: String },
}

fn default_scoped_tools() -> Vec<String> {
    CapabilityProfile::standard_tools()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_serde_round_trip() {
        let events = vec![
            AgentEvent::TurnStarted,
            AgentEvent::ItemStartedKind {
                item_type: "message".into(),
            },
            AgentEvent::ItemStarted {
                item: Item::ShellCommand {
                    command: "pwd".into(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            },
            AgentEvent::MessageDelta {
                text: "hello".into(),
            },
            AgentEvent::ToolOutputDelta {
                item_id: "item-1".into(),
                text: "output".into(),
            },
            AgentEvent::ToolCall {
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
            AgentEvent::ApprovalRequest {
                id: "req-1".into(),
                command: "rm -rf /tmp/test".into(),
            },
            AgentEvent::ItemCompletedKind,
            AgentEvent::ItemCompleted {
                item: Item::AgentReasoning {
                    content: "done".into(),
                },
            },
            AgentEvent::TokenUsage {
                usage: TokenUsage::default(),
            },
            AgentEvent::Warning {
                message: "careful".into(),
            },
            AgentEvent::TurnCompleted {
                output: "done".into(),
            },
            AgentEvent::Error {
                message: "oops".into(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: AgentEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, parsed);
        }
    }

    #[test]
    fn agent_event_tagged_format() {
        let event = AgentEvent::ToolCall {
            name: "bash".into(),
            input: serde_json::json!({"cmd": "ls"}),
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["name"], "bash");
    }

    #[test]
    fn approval_decision_serde_round_trip() {
        let decisions = vec![
            ApprovalDecision::Accept,
            ApprovalDecision::Reject {
                reason: "dangerous".into(),
            },
        ];

        for decision in decisions {
            let json = serde_json::to_string(&decision).unwrap();
            let parsed: ApprovalDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(decision, parsed);
        }
    }
}
