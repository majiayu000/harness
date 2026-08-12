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
#![cfg_attr(not(test), deny(clippy::disallowed_types))]
#![cfg_attr(test, allow(clippy::disallowed_types))]

pub mod alerting;
pub mod assets;
pub(crate) mod command_safety;
pub mod complexity_router;
pub mod contract_validator;
pub mod dashboard;
pub mod db;
pub mod event_replay;
pub(crate) mod feishu_client;
pub(crate) mod github_auth;
pub(crate) mod github_client;
pub(crate) mod github_pr_hygiene;
pub(crate) mod github_pr_merge;
pub(crate) mod github_pr_snapshot;
pub mod handlers;
mod hook_circuit_breaker;
pub mod hook_enforcer;
pub mod http;
pub mod intake;
pub(crate) mod isolation_health;
pub mod memory_monitor;
pub mod notify;
pub(crate) mod observation_compression;
pub mod overview;
pub mod parallel_dispatch;
pub mod periodic_reviewer;
pub mod reconciliation;
pub use harness_workflow::plan_db;
pub mod post_validator;
pub(crate) mod postgres_catalog;
pub mod project_registry;
pub mod quality_trigger;
pub mod redact;
pub mod router;
pub(crate) mod runtime_circuit_breaker;
pub mod runtime_hosts;
pub mod runtime_hosts_state;
pub mod runtime_project_cache;
pub mod runtime_project_cache_state;
pub(crate) mod runtime_projection;
pub mod runtime_state_store;
pub mod scheduler;
pub mod self_evolution;
pub mod server;
pub mod services;
pub mod skill_governor;
pub mod stdio;
pub mod task_db;
pub(crate) mod task_queue;
pub mod task_runner;
pub mod thread_manager;
pub mod trusted_proxy;
pub mod webhook;
pub mod websocket;
#[cfg(test)]
pub(crate) mod workflow_runtime_plan_issue;
pub(crate) mod workflow_runtime_policy;
pub(crate) mod workflow_runtime_pr_feedback;
pub(crate) mod workflow_runtime_submission;
pub(crate) mod workflow_runtime_worker;
pub mod workspace;
pub(crate) mod workspace_lease_store;
pub(crate) mod workspace_pool;

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
mod runtime_hosts_tests;

#[cfg(test)]
mod historical_replay_tests;

#[cfg(test)]
mod runtime_state_store_tests;

#[cfg(test)]
mod thread_manager_tests;

#[cfg(test)]
mod quality_trigger_tests;
