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

pub mod checkpoint;
pub mod circuit_breaker;
pub mod complexity_router;
pub mod contract_validator;
pub mod dashboard;
pub mod db;
pub mod handlers;
pub mod hook_enforcer;
pub mod http;
pub mod intake;
pub mod notify;
pub mod parallel_dispatch;
pub mod periodic_reviewer;
pub mod plan_db;
pub mod post_validator;
pub mod project_registry;
pub mod quality_trigger;
pub mod review_store;
pub mod router;
pub mod rule_enforcer;
pub mod scheduler;
pub mod self_evolution;
pub mod server;
pub mod services;
pub mod skill_governor;
pub mod stdio;
pub mod task_db;
pub mod task_executor;
pub mod task_queue;
pub mod task_runner;
pub mod thread_db;
pub mod thread_manager;
pub mod trusted_proxy;
pub mod webhook;
pub mod websocket;
pub mod workspace;

#[cfg(test)]
pub(crate) mod test_helpers;
