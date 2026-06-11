use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCommandStatus {
    Pending,
    Dispatching,
    Dispatched,
    HandledInline,
    Completed,
    Failed,
    Blocked,
    Cancelled,
    Skipped,
}

impl WorkflowCommandStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatching => "dispatching",
            Self::Dispatched => "dispatched",
            Self::HandledInline => "handled_inline",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

impl fmt::Display for WorkflowCommandStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<&str> for WorkflowCommandStatus {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl TryFrom<&str> for WorkflowCommandStatus {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "dispatching" => Ok(Self::Dispatching),
            "dispatched" => Ok(Self::Dispatched),
            "handled_inline" => Ok(Self::HandledInline),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "blocked" => Ok(Self::Blocked),
            "cancelled" => Ok(Self::Cancelled),
            "skipped" => Ok(Self::Skipped),
            other => anyhow::bail!("unknown workflow command status: {other}"),
        }
    }
}
