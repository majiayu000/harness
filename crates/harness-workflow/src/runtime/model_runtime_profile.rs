use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    CodexExec,
    CodexJsonrpc,
    ClaudeCode,
    AnthropicApi,
    RemoteHost,
    OpenCode,
    Cursor,
}

impl RuntimeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodexExec => "codex_exec",
            Self::CodexJsonrpc => "codex_jsonrpc",
            Self::ClaudeCode => "claude_code",
            Self::AnthropicApi => "anthropic_api",
            Self::RemoteHost => "remote_host",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeProfile {
    pub name: String,
    pub kind: RuntimeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl RuntimeProfile {
    pub fn new(name: impl Into<String>, kind: RuntimeKind) -> Self {
        Self {
            name: name.into(),
            kind,
            model: None,
            reasoning_effort: None,
            sandbox: None,
            approval_policy: None,
            max_turns: None,
            timeout_secs: None,
        }
    }
}
