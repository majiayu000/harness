use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub total_events: u64,
    pub pass_count: u64,
    pub warn_count: u64,
    pub block_count: u64,
    pub avg_duration_ms: f64,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
}
