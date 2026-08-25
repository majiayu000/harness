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

pub(crate) mod alerting;
pub(crate) mod assets;
pub(crate) mod command_safety;
pub(crate) mod complexity_router;
pub(crate) mod contract_validator;
pub(crate) mod dashboard;
pub(crate) mod db;
pub(crate) mod eval_credentials;
pub(crate) mod event_replay;
pub(crate) mod feishu_client;
pub(crate) mod github_auth;
pub(crate) mod github_client;
pub(crate) mod github_pr_hygiene;
pub(crate) mod github_pr_merge;
pub(crate) mod github_pr_snapshot;
pub(crate) mod handlers;
mod hook_circuit_breaker;
pub(crate) mod hook_enforcer;
pub(crate) mod http;
pub(crate) mod intake;
pub(crate) mod isolation_health;
pub(crate) mod memory_monitor;
pub(crate) mod notify;
pub(crate) mod observation_compression;
pub(crate) mod overview;
#[cfg(test)]
pub(crate) mod parallel_dispatch;
pub(crate) mod periodic_reviewer;
pub mod reconciliation;
pub(crate) use harness_workflow::plan_db;
pub(crate) mod post_validator;
pub(crate) mod postgres_catalog;
pub mod project_registry;
pub(crate) mod quality_trigger;
#[cfg(test)]
pub(crate) mod redact;
pub(crate) mod router;
pub(crate) mod runtime_circuit_breaker;
pub(crate) mod runtime_hosts;
pub(crate) mod runtime_hosts_state;
pub(crate) mod runtime_project_cache;
pub(crate) mod runtime_project_cache_state;
pub(crate) mod runtime_projection;
pub(crate) mod runtime_state_store;
pub(crate) mod scheduler;
pub(crate) mod self_evolution;
pub mod server;
pub(crate) mod services;
pub(crate) mod skill_governor;
pub(crate) mod stdio;
pub(crate) mod task_db;
pub(crate) mod task_queue;
pub(crate) mod task_runner;
pub mod thread_manager;
#[cfg(test)]
pub(crate) mod trusted_proxy;
pub(crate) mod validation_executor;
pub(crate) mod webhook;
pub(crate) mod websocket;
pub(crate) mod websocket_dispatch;
#[cfg(test)]
pub(crate) mod websocket_test_support;
#[cfg(test)]
pub(crate) mod workflow_runtime_plan_issue;
pub(crate) mod workflow_runtime_policy;
pub(crate) mod workflow_runtime_pr_feedback;
pub(crate) mod workflow_runtime_submission;
pub(crate) mod workflow_runtime_worker;
pub(crate) mod workspace;
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
