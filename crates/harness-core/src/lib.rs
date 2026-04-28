pub mod agent;
pub mod agents_md;
pub mod capability;
pub mod config;
pub mod db;
pub mod db_pg;
pub mod error;
pub mod interceptor;
pub mod lang_detect;
pub mod prompts;
pub mod shell_safety;
pub mod tool_isolation;
pub mod types;

pub use config::misc::OtelExporter;
pub use types::{
    AutoFixAttempt, AutoFixReport, Decision, Event, EventFilters, ExternalSignal, RuleId,
    SessionId, Severity,
};

#[cfg(test)]
pub(crate) mod test_support {
    pub static PROCESS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub fn process_env_lock() -> std::sync::MutexGuard<'static, ()> {
        PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
