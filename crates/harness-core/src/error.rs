use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HarnessError {
    #[error("thread not found: {0}")]
    ThreadNotFound(String),

    #[error("turn not found: {0}")]
    TurnNotFound(String),

    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("draft not found: {0}")]
    DraftNotFound(String),

    #[error("skill not found: {0}")]
    SkillNotFound(String),

    #[error("exec plan not found: {0}")]
    ExecPlanNotFound(String),

    #[error("rule not found: {0}")]
    RuleNotFound(String),

    #[error("invalid state: {0}")]
    InvalidState(String),

    #[error("budget exceeded: spent ${spent:.2}, limit ${limit:.2}")]
    BudgetExceeded { spent: f64, limit: f64 },

    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("agent execution failed: {0}")]
    AgentExecution(String),

    #[error("agent quota exhausted: {0}")]
    QuotaExhausted(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("persistence error: {0}")]
    Persistence(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Sandbox(#[from] SandboxError),

    #[error(transparent)]
    TaskDbDecode(#[from] TaskDbDecodeError),

    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox mode `{mode}` is unsupported on `{platform}`")]
    UnsupportedPlatform {
        mode: crate::SandboxMode,
        platform: &'static str,
    },
    #[error("sandbox tool not found: {0}")]
    MissingTool(&'static str),
    #[error("invalid sandbox path `{path}`: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("sandbox helper `{helper}` does not support mode `{mode}`")]
    InvalidHelperMode {
        helper: &'static str,
        mode: crate::SandboxMode,
    },
}

#[derive(Debug, Error)]
pub enum TaskDbDecodeError {
    #[error("failed to deserialize rounds for task `{task_id}`")]
    RoundsDeserialize {
        task_id: String,
        #[source]
        source: serde_json::Error,
    },
}

pub type Error = HarnessError;
pub type Result<T> = std::result::Result<T, HarnessError>;
