//! Startup reconciliation for process-owned Docker resources.

use harness_core::error::HarnessError;

/// Remove resources whose owning Harness process is no longer alive.
/// Only labeled resources with expected Harness name prefixes are eligible.
pub fn reconcile_stale_resources() -> Result<(), HarnessError> {
    crate::spawn_contract::docker_ownership::reconcile_stale_resources()
}
