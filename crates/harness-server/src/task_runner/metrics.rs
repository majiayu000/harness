use std::collections::HashMap;

/// In-memory cache + SQLite persistence.
/// Per-project done/failed task counts derived from the in-memory cache.
#[derive(Debug, Default, Clone)]
pub struct ProjectCounts {
    pub done: u64,
    pub failed: u64,
}

/// Combined global and per-project done/failed counts produced by a single
/// cache scan, avoiding both full task cloning and double iteration.
#[derive(Debug)]
pub struct DashboardCounts {
    pub global_done: u64,
    pub global_failed: u64,
    pub by_project: HashMap<String, ProjectCounts>,
}

/// Lightweight inputs collected for LLM metrics computation.
///
/// Bounded inputs for LLM metrics computation, collected in two O(1) phases:
/// cache iteration (active tasks) then bounded SQL queries (terminal tasks).
#[derive(Debug)]
pub struct LlmMetricsInputs {
    /// Non-zero turn counts from both active (cache) and terminal (DB) tasks.
    pub turn_counts: Vec<u32>,
    /// First real first-token latency per task (milliseconds), from cache and DB.
    pub first_token_latencies: Vec<u64>,
}
