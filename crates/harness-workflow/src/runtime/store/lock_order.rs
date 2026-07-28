//! Canonical row/advisory-lock order for the workflow runtime store, and the
//! retry policy for the aborts that violating it used to produce.
//!
//! # The order
//!
//! Any transaction that takes `SELECT ... FOR UPDATE` row locks on more than
//! one of these tables MUST acquire them parent-to-child:
//!
//! 1. `workflow_instances`
//! 2. `workflow_commands`
//! 3. `runtime_jobs`
//!
//! A transaction may skip levels (locking only commands and jobs is fine); it
//! may not invert them. When a later lock's key is only reachable through a
//! child row, read that key with a plain `SELECT` first — an unlocked read
//! takes no row lock, so it cannot participate in a cycle — then take the
//! locks in order. `commands.rs` and `activity_completion.rs` both do this to
//! resolve `workflow_id` before locking the instance.
//!
//! Workflow event writers have one additional invariant: lock the referenced
//! `workflow_instances` row first, then take the per-workflow event-sequence
//! advisory lock. The event foreign key takes a parent `KEY SHARE` lock during
//! insert; taking the advisory lock first would let it deadlock with a
//! transition that already holds the instance `FOR UPDATE` and is waiting for
//! the same advisory lock. All production event inserts therefore go through
//! `insert_event_tx_with_id`.
//!
//! `lock_order_tests.rs` enforces this by scanning the runtime store sources
//! and the store implementations that live in sibling runtime modules.
//!
//! # Why
//!
//! Command dispatch locked `workflow_instances` then `workflow_commands`,
//! while activity completion locked `workflow_commands`, then `runtime_jobs`,
//! then `workflow_instances`. A dispatcher finishing a command while a worker
//! committed a completion on the same workflow is a textbook ABBA
//! interleaving: PostgreSQL breaks the cycle by aborting one side with
//! SQLSTATE 40P01, which surfaced as an opaque error and failed the runtime
//! job.

use std::time::Duration;

/// Tables that take part in the ordered lock hierarchy, ranked parent-first.
///
/// The machine-readable form of the order documented above; only the lint
/// consumes it, so it is not compiled into the shipped store.
#[cfg(test)]
pub(super) const LOCK_HIERARCHY: [&str; 3] =
    ["workflow_instances", "workflow_commands", "runtime_jobs"];

/// Helpers that take a hierarchy row lock on their caller's behalf, and the
/// table each one locks.
///
/// The lint reads one function at a time, so a lock taken inside a callee is
/// invisible to it — and that is exactly the shape of the ABBA this module
/// documents: the completion path's `workflow_instances` lock lived inside
/// `apply_runtime_completion_decision_tx`. Entries here make those calls count
/// as lock sites at the call site.
///
/// Add an entry whenever a `_tx` helper starts taking a hierarchy row lock.
#[cfg(test)]
pub(super) const LOCK_TAKING_HELPERS: [(&str, &str); 8] = [
    ("select_instance_for_update_tx", "workflow_instances"),
    ("lock_instance_for_update_tx", "workflow_instances"),
    ("lock_instance_for_event_sequence_tx", "workflow_instances"),
    ("apply_runtime_completion_decision_tx", "workflow_instances"),
    ("insert_event_tx", "workflow_instances"),
    ("insert_event_tx_with_id", "workflow_instances"),
    ("skip_superseded_active_commands_tx", "workflow_commands"),
    ("cancel_unfinished_runtime_jobs_tx", "runtime_jobs"),
];

/// `deadlock_detected` — PostgreSQL aborted this transaction to break a cycle.
const SQLSTATE_DEADLOCK_DETECTED: &str = "40P01";
/// `serialization_failure` — the same class of "retry the whole transaction"
/// abort, raised under concurrent updates rather than a lock cycle.
const SQLSTATE_SERIALIZATION_FAILURE: &str = "40001";

/// Number of times a transaction is re-run after a deadlock abort before the
/// error is surfaced to the caller.
const MAX_DEADLOCK_RETRIES: usize = 3;
/// Base backoff between attempts. Deadlock resolution needs the *other*
/// transaction to finish, which is fast; this only avoids a tight loop.
const RETRY_BACKOFF: Duration = Duration::from_millis(25);

/// True when the error is a PostgreSQL transaction abort that is safe to retry
/// by re-running the whole transaction from the start.
///
/// Both codes mean "this transaction did nothing and was rolled back", so a
/// retry is not a partial re-application.
pub(super) fn is_retryable_transaction_abort(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .and_then(|error| error.as_database_error())
        .and_then(|error| error.code().map(|code| code.into_owned()))
        .is_some_and(|code| {
            code == SQLSTATE_DEADLOCK_DETECTED || code == SQLSTATE_SERIALIZATION_FAILURE
        })
}

/// Runs `operation`, re-running it from scratch when PostgreSQL aborts the
/// transaction with a deadlock or serialization failure.
///
/// `operation` must own its whole transaction: it is called again from the
/// beginning, so it cannot hold state across attempts.
pub(super) async fn retry_on_transaction_abort<F, Fut, T>(
    label: &str,
    mut operation: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0;
    loop {
        let error = match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        if !is_retryable_transaction_abort(&error) || attempt >= MAX_DEADLOCK_RETRIES {
            return Err(error);
        }
        attempt += 1;
        tracing::warn!(
            operation = label,
            attempt,
            max_attempts = MAX_DEADLOCK_RETRIES,
            error = %error,
            "workflow store: transaction aborted by PostgreSQL; retrying"
        );
        tokio::time::sleep(RETRY_BACKOFF * attempt as u32).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// sqlx exposes no public constructor for a `PgDatabaseError`, so the
    /// classifier is driven through the real `sqlx::Error::Database` downcast
    /// path with a `DatabaseError` implementation that only carries a SQLSTATE.
    #[derive(Debug)]
    struct SqlStateError(&'static str);

    impl std::fmt::Display for SqlStateError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "database error {}", self.0)
        }
    }

    impl std::error::Error for SqlStateError {}

    impl sqlx::error::DatabaseError for SqlStateError {
        fn message(&self) -> &str {
            "database error"
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
    }

    fn sqlstate_error(code: &'static str) -> anyhow::Error {
        anyhow::Error::new(sqlx::Error::Database(Box::new(SqlStateError(code))))
    }

    fn deadlock_error() -> anyhow::Error {
        sqlstate_error(SQLSTATE_DEADLOCK_DETECTED)
    }

    #[test]
    fn classifies_deadlock_as_retryable() {
        assert!(is_retryable_transaction_abort(&deadlock_error()));
    }

    #[test]
    fn classifies_serialization_failure_as_retryable() {
        assert!(is_retryable_transaction_abort(&sqlstate_error(
            SQLSTATE_SERIALIZATION_FAILURE
        )));
    }

    #[test]
    fn does_not_classify_unique_violation_as_retryable() {
        assert!(!is_retryable_transaction_abort(&sqlstate_error("23505")));
    }

    #[test]
    fn does_not_classify_plain_errors_as_retryable() {
        assert!(!is_retryable_transaction_abort(&anyhow::anyhow!(
            "runtime job not found"
        )));
    }

    #[tokio::test]
    async fn retries_deadlock_until_success() {
        let attempts = AtomicUsize::new(0);
        let value = retry_on_transaction_abort("test", || async {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(deadlock_error());
            }
            Ok(7)
        })
        .await
        .expect("retry should succeed once the conflict clears");
        assert_eq!(value, 7);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn surfaces_deadlock_after_exhausting_retries() {
        let attempts = AtomicUsize::new(0);
        let result: anyhow::Result<()> = retry_on_transaction_abort("test", || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(deadlock_error())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_DEADLOCK_RETRIES + 1);
    }

    #[tokio::test]
    async fn does_not_retry_non_abort_errors() {
        let attempts = AtomicUsize::new(0);
        let result: anyhow::Result<()> = retry_on_transaction_abort("test", || async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!("workflow command not found"))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
