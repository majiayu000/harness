use harness_core::types::{
    DraftId, Event, EventFilters, ExecPlanId, MetricFilters, ProjectId, SkillId, ThreadId, TurnId,
};
use serde::{de, Deserialize, Deserializer, Serialize};
use std::path::PathBuf;

/// All JSON-RPC 2.0 methods supported by the Harness agent-facing protocol.
///
/// # Transport contract
///
/// This enum covers the **agent / data-plane** surface only (JSON-RPC over stdio,
/// WebSocket, or HTTP `/rpc`).  Task and project management (the *operator /
/// control-plane* surface) is exclusively available over HTTP REST:
///
/// | Control-plane capability | HTTP endpoint |
/// |--------------------------|---------------|
/// | Enqueue a task           | `POST /tasks` |
/// | List tasks               | `GET  /tasks` |
/// | Batch-create tasks       | `POST /tasks/batch` |
/// | Stream task output       | `GET  /tasks/{id}/stream` |
/// | Register a project       | `POST /projects` |
/// | List projects            | `GET  /projects` |
/// | Dashboard API            | `GET  /api/dashboard` |
///
/// See `docs/api-contract.md` for the full transport role description.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    // === Initialization ===
    Initialize,
    /// Sent by the client after receiving an `initialize` response to confirm
    /// the handshake is complete.  No params required.
    Initialized,

    // === Thread management ===
    ThreadStart {
        cwd: PathBuf,
    },
    ThreadResume {
        thread_id: ThreadId,
    },
    ThreadFork {
        thread_id: ThreadId,
        from_turn: Option<TurnId>,
    },
    ThreadList,
    ThreadDelete {
        thread_id: ThreadId,
    },
    ThreadCompact {
        thread_id: ThreadId,
    },

    // === Turn control ===
    TurnStart {
        thread_id: ThreadId,
        input: String,
    },
    TurnSteer {
        turn_id: TurnId,
        instruction: String,
    },
    TurnCancel {
        turn_id: TurnId,
    },
    TurnStatus {
        turn_id: TurnId,
    },

    // === GC Agent ===
    GcRun {
        project_id: Option<ProjectId>,
    },
    GcStatus,
    GcDrafts {
        project_id: Option<ProjectId>,
    },
    GcAdopt {
        draft_id: DraftId,
    },
    GcReject {
        draft_id: DraftId,
        reason: Option<String>,
    },

    // === Skill system ===
    SkillCreate {
        name: String,
        content: String,
    },
    SkillList {
        query: Option<String>,
    },
    SkillGet {
        skill_id: SkillId,
    },
    SkillDelete {
        skill_id: SkillId,
    },

    // === Rule engine ===
    RuleLoad {
        project_root: PathBuf,
    },
    RuleCheck {
        project_root: PathBuf,
        files: Option<Vec<PathBuf>>,
    },

    // === ExecPlan ===
    ExecPlanInit {
        spec: String,
        project_root: PathBuf,
    },
    ExecPlanUpdate {
        plan_id: ExecPlanId,
        updates: serde_json::Value,
    },
    ExecPlanStatus {
        plan_id: ExecPlanId,
    },

    // === Observability ===
    EventLog {
        event: Event,
    },
    EventQuery {
        filters: EventFilters,
    },
    MetricsCollect {
        project_root: PathBuf,
    },
    MetricsQuery {
        filters: MetricFilters,
    },

    // === Task classification ===
    TaskClassify {
        prompt: String,
        issue: Option<u64>,
        pr: Option<u64>,
    },

    // === Learn feedback loop ===
    LearnRules {
        project_root: PathBuf,
    },
    LearnSkills {
        project_root: PathBuf,
    },

    // === Health & Stats ===
    HealthCheck {
        project_root: PathBuf,
    },
    StatsQuery {
        since: Option<chrono::DateTime<chrono::Utc>>,
        until: Option<chrono::DateTime<chrono::Utc>>,
    },

    // === Agent management ===
    AgentList,

    // === VibeGuard ===
    Preflight {
        project_root: PathBuf,
        task_description: String,
    },
    CrossReview {
        project_root: PathBuf,
        target: String,
        max_rounds: Option<u32>,
    },
}

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(flatten)]
    pub method: Method,
}

impl<'de> Deserialize<'de> for RpcRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawRpcRequest {
            jsonrpc: String,
            #[serde(default)]
            id: Option<serde_json::Value>,
            method: String,
            #[serde(default)]
            params: Option<serde_json::Value>,
        }

        let RawRpcRequest {
            jsonrpc,
            id,
            method,
            params,
        } = RawRpcRequest::deserialize(deserializer)?;

        // Normalize slash-style method names to snake_case for serde:
        // "thread/start" -> "thread_start", "exec_plan/init" -> "exec_plan_init"
        let method = method.replace('/', "_");

        let method = if method == "initialized" {
            if let Some(p) = &params {
                let is_valid = p.is_null() || p.as_object().is_some_and(|m| m.is_empty());
                if !is_valid {
                    return Err(de::Error::custom("`initialized` does not accept params"));
                }
            }
            Method::Initialized
        } else {
            let mut raw_method = serde_json::Map::new();
            raw_method.insert("method".to_string(), serde_json::Value::String(method));
            if let Some(params) = params {
                raw_method.insert("params".to_string(), params);
            }
            serde_json::from_value::<Method>(serde_json::Value::Object(raw_method))
                .map_err(de::Error::custom)?
        };

        Ok(Self {
            jsonrpc,
            id,
            method,
        })
    }
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

impl Method {
    /// Returns the canonical slash-style method name (e.g. `"thread/start"`).
    pub fn method_name(&self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Initialized => "initialized",
            Self::ThreadStart { .. } => "thread/start",
            Self::ThreadResume { .. } => "thread/resume",
            Self::ThreadFork { .. } => "thread/fork",
            Self::ThreadList => "thread/list",
            Self::ThreadDelete { .. } => "thread/delete",
            Self::ThreadCompact { .. } => "thread/compact",
            Self::TurnStart { .. } => "turn/start",
            Self::TurnSteer { .. } => "turn/steer",
            Self::TurnCancel { .. } => "turn/cancel",
            Self::TurnStatus { .. } => "turn/status",
            Self::GcRun { .. } => "gc/run",
            Self::GcStatus => "gc/status",
            Self::GcDrafts { .. } => "gc/drafts",
            Self::GcAdopt { .. } => "gc/adopt",
            Self::GcReject { .. } => "gc/reject",
            Self::SkillCreate { .. } => "skill/create",
            Self::SkillList { .. } => "skill/list",
            Self::SkillGet { .. } => "skill/get",
            Self::SkillDelete { .. } => "skill/delete",
            Self::RuleLoad { .. } => "rule/load",
            Self::RuleCheck { .. } => "rule/check",
            Self::ExecPlanInit { .. } => "exec_plan/init",
            Self::ExecPlanUpdate { .. } => "exec_plan/update",
            Self::ExecPlanStatus { .. } => "exec_plan/status",
            Self::EventLog { .. } => "event/log",
            Self::EventQuery { .. } => "event/query",
            Self::MetricsCollect { .. } => "metrics/collect",
            Self::MetricsQuery { .. } => "metrics/query",
            Self::TaskClassify { .. } => "task/classify",
            Self::LearnRules { .. } => "learn/rules",
            Self::LearnSkills { .. } => "learn/skills",
            Self::HealthCheck { .. } => "health/check",
            Self::StatsQuery { .. } => "stats/query",
            Self::AgentList => "agent/list",
            Self::Preflight { .. } => "preflight",
            Self::CrossReview { .. } => "cross_review",
        }
    }
}

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// Application-specific semantic error codes (server error range: -32000 to -32099)
pub const NOT_FOUND: i32 = -32001;
pub const CONFLICT: i32 = -32002;
pub const NOT_INITIALIZED: i32 = -32003;
pub const STORAGE_ERROR: i32 = -32004;
pub const AGENT_ERROR: i32 = -32005;
pub const VALIDATION_ERROR: i32 = -32006;
