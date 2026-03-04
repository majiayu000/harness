use harness_core::{
    DraftId, EventFilters, ExecPlanId, MetricFilters, ProjectId, SkillId, ThreadId, TurnId, Event,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// All JSON-RPC 2.0 methods supported by the Harness App Server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    // === Initialization ===
    Initialize,
    /// Sent by the client after receiving an `initialize` response to confirm
    /// the handshake is complete.  No params required.
    Initialized,

    // === Thread management ===
    ThreadStart { cwd: PathBuf },
    ThreadResume { thread_id: ThreadId },
    ThreadFork { thread_id: ThreadId, from_turn: Option<TurnId> },
    ThreadList,
    ThreadDelete { thread_id: ThreadId },
    ThreadCompact { thread_id: ThreadId },

    // === Turn control ===
    TurnStart { thread_id: ThreadId, input: String },
    TurnSteer { turn_id: TurnId, instruction: String },
    TurnCancel { turn_id: TurnId },
    TurnStatus { turn_id: TurnId },

    // === GC Agent ===
    GcRun { project_id: Option<ProjectId> },
    GcStatus,
    GcDrafts { project_id: Option<ProjectId> },
    GcAdopt { draft_id: DraftId },
    GcReject { draft_id: DraftId, reason: Option<String> },

    // === Skill system ===
    SkillCreate { name: String, content: String },
    SkillList { query: Option<String> },
    SkillGet { skill_id: SkillId },
    SkillDelete { skill_id: SkillId },

    // === Rule engine ===
    RuleLoad { project_root: PathBuf },
    RuleCheck { project_root: PathBuf, files: Option<Vec<PathBuf>> },

    // === ExecPlan ===
    ExecPlanInit { spec: String, project_root: PathBuf },
    ExecPlanUpdate { plan_id: ExecPlanId, updates: serde_json::Value },
    ExecPlanStatus { plan_id: ExecPlanId },

    // === Observability ===
    EventLog { event: Event },
    EventQuery { filters: EventFilters },
    MetricsCollect { project_root: PathBuf },
    MetricsQuery { filters: MetricFilters },

    // === Task classification ===
    TaskClassify { prompt: String, issue: Option<u64>, pr: Option<u64> },

    // === Learn feedback loop ===
    LearnRules { project_root: PathBuf },
    LearnSkills { project_root: PathBuf },

    // === Health & Stats ===
    HealthCheck { project_root: PathBuf },
    StatsQuery {
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    },

    // === VibeGuard ===
    Preflight { project_root: PathBuf, task_description: String },
    CrossReview { project_root: PathBuf, target: String, max_rounds: Option<u32> },
}

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(flatten)]
    pub method: Method,
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcResponse {
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
