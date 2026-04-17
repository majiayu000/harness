pub(crate) mod auth;
pub(crate) mod builders;
pub(crate) mod handlers;
pub(crate) mod helpers;
pub(crate) mod init;
pub(crate) mod intake_auth;
pub(crate) mod rate_limit;
pub(crate) mod server;
pub(crate) mod startup_recovery;
pub(crate) mod task_routes;
pub(crate) mod types;

pub(crate) use helpers::{parse_pr_num_from_url, resolve_reviewer};
pub use init::build_app_state;
pub(crate) use init::build_completion_callback;
pub use server::serve;
pub use types::{
    AppState, ConcurrencyServices, CoreServices, EngineServices, IntakeServices,
    NotificationServices, ObservabilityServices,
};

#[cfg(test)]
mod startup_tests;

#[cfg(test)]
mod tests;
