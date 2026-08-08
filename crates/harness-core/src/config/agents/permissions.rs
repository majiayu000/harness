use serde::{Deserialize, Serialize};

/// Preset capability profile that determines which tools an agent may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityProfile {
    /// Read-only access: Read, Grep, Glob only. Suitable for GC/review agents.
    ReadOnly,
    /// Standard access: Read, Write, Edit, Bash. Suitable for implementation agents.
    #[default]
    Standard,
    /// Full access — all tools. Requires an explicit opt-up.
    Full,
}

impl CapabilityProfile {
    /// Returns the explicit tool list for this profile, or `None` for `Full`
    /// (meaning no restriction is applied to the CLI invocation).
    pub fn tools(self) -> Option<Vec<String>> {
        match self {
            CapabilityProfile::ReadOnly => Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
            ]),
            CapabilityProfile::Standard => Some(vec![
                "Read".to_string(),
                "Write".to_string(),
                "Edit".to_string(),
                "Bash".to_string(),
            ]),
            CapabilityProfile::Full => None,
        }
    }

    /// Human-readable description injected into the agent prompt as context.
    pub fn prompt_note(self) -> Option<&'static str> {
        match self {
            CapabilityProfile::ReadOnly => Some(
                "Tool restriction: you are operating in read-only mode. \
                 Only Read, Grep, and Glob are permitted. \
                 Do NOT call Write, Edit, Bash, or any other tool.",
            ),
            CapabilityProfile::Standard => Some(
                "Tool restriction: you are operating in standard mode. \
                 Only Read, Write, Edit, and Bash are permitted. \
                 Do NOT call tools outside this list.",
            ),
            CapabilityProfile::Full => None,
        }
    }

    pub fn permission_mode(self) -> AgentPermissionMode {
        match self {
            CapabilityProfile::ReadOnly | CapabilityProfile::Standard => {
                AgentPermissionMode::Scoped
            }
            CapabilityProfile::Full => AgentPermissionMode::Full,
        }
    }
}

/// Whether an agent request is restricted to an explicit tool set or may use
/// the backend's unrestricted permission mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionMode {
    /// Restrict the request to its resolved tool allowlist.
    #[default]
    Scoped,
    /// Allow unrestricted backend tool access. This must be configured
    /// explicitly through [`CapabilityProfile::Full`].
    Full,
}

/// Controls how much autonomy the agent has when executing tasks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Agent suggests changes but does not apply them.
    Suggest,
    /// Agent can edit files but requires human approval for shell commands.
    #[default]
    AutoEdit,
    /// Agent has full autonomy — no approval gates.
    FullAuto,
}

/// OS-level sandbox mode for agent subprocess execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    ReadOnlyWithNetwork,
    WorkspaceWrite,
    #[default]
    DangerFullAccess,
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::ReadOnlyWithNetwork => "read-only-with-network",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        };
        write!(f, "{value}")
    }
}
