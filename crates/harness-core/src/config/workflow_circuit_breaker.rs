use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowRuntimeConfig {
    #[serde(default)]
    pub circuit_breaker: RuntimeCircuitBreakerPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeCircuitBreakerPolicy {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_consecutive_failures")]
    pub consecutive_failures: u32,
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    #[serde(default = "default_backoff_factor")]
    pub backoff_factor: f64,
    #[serde(default = "default_max_cooldown_secs")]
    pub max_cooldown_secs: u64,
}

impl Default for RuntimeCircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            consecutive_failures: default_consecutive_failures(),
            cooldown_secs: default_cooldown_secs(),
            backoff_factor: default_backoff_factor(),
            max_cooldown_secs: default_max_cooldown_secs(),
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_consecutive_failures() -> u32 {
    5
}

fn default_cooldown_secs() -> u64 {
    600
}

fn default_backoff_factor() -> f64 {
    2.0
}

fn default_max_cooldown_secs() -> u64 {
    7200
}
