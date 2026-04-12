#![allow(
    clippy::field_reassign_with_default,
    clippy::items_after_test_module,
    clippy::manual_is_multiple_of,
    clippy::manual_pattern_char_comparison,
    clippy::new_without_default,
    clippy::too_many_arguments,
    clippy::unnecessary_cast,
    clippy::unnecessary_to_owned
)]

// Modules extracted to `harness-workflow`; re-exported so existing `crate::*`
// paths inside this crate continue to resolve without modification.
pub use harness_workflow::checkpoint;
pub use harness_workflow::circuit_breaker;
pub mod complexity_router;
pub mod contract_validator;
pub mod dashboard;
pub mod db;
pub mod event_replay;
pub mod handlers;
pub mod hook_enforcer;
pub mod http;
pub mod intake;
pub mod memory_monitor;
pub mod notify;
pub mod parallel_dispatch;
pub mod periodic_reviewer;
pub use harness_workflow::plan_db;
pub mod post_validator;
pub mod project_registry;
pub mod q_value_store;
pub mod quality_trigger;
pub mod review_store;
pub mod router;
pub mod rule_enforcer;
pub mod runtime_hosts;
pub mod runtime_hosts_state;
pub mod runtime_project_cache;
pub mod runtime_project_cache_state;
pub mod runtime_state_store;
pub mod scheduler;
pub mod self_evolution;
pub mod server;
pub mod services;
pub mod skill_governor;
pub mod stdio;
pub mod task_db;
pub mod task_executor;
pub use harness_workflow::task_queue;
pub mod task_runner;
pub mod thread_db;
pub mod thread_manager;
pub mod trusted_proxy;
pub mod webhook;
pub mod websocket;
pub mod workspace;

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
mod runtime_hosts_tests;

#[cfg(test)]
mod runtime_state_store_tests;

#[cfg(test)]
mod thread_manager_tests;

#[cfg(test)]
mod quality_trigger_tests;
