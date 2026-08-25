use super::*;
use sqlx::pool::PoolConnection;
use sqlx::Postgres;
use tokio::sync::{watch, OwnedSemaphorePermit};

const REPOSITORY_LEASE_HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const REPOSITORY_LEASE_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const REPOSITORY_LEASE_HEARTBEAT_FAILURE_LIMIT: u32 = 3;

fn repository_heartbeat_failure_is_terminal(consecutive_failures: u32) -> bool {
    consecutive_failures >= REPOSITORY_LEASE_HEARTBEAT_FAILURE_LIMIT
}

fn repository_heartbeat_error_is_definitive(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(error) => error
            .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
            .is_some_and(|error| repository_heartbeat_severity_is_definitive(error.severity())),
        _ => false,
    }
}

fn repository_heartbeat_severity_is_definitive(severity: sqlx::postgres::PgSeverity) -> bool {
    matches!(
        severity,
        sqlx::postgres::PgSeverity::Fatal | sqlx::postgres::PgSeverity::Panic
    )
}

pub(crate) struct RepositoryWriteLease {
    _connection: Arc<tokio::sync::Mutex<Option<PoolConnection<Postgres>>>>,
    _slot_permit: Arc<tokio::sync::Mutex<Option<OwnedSemaphorePermit>>>,
    mode: RepositoryLeaseMode,
    state_tx: watch::Sender<RepositoryLeaseState>,
    state: watch::Receiver<RepositoryLeaseState>,
    liveness_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositoryLeaseMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepositoryLeaseState {
    Healthy,
    Revoking,
    Lost,
    Released,
}

fn transition_repository_lease_state(
    sender: &watch::Sender<RepositoryLeaseState>,
    from: RepositoryLeaseState,
    to: RepositoryLeaseState,
) -> bool {
    sender.send_if_modified(|state| {
        if *state == from {
            *state = to;
            true
        } else {
            false
        }
    })
}

impl RepositoryWriteLease {
    fn monitored(
        connection: PoolConnection<Postgres>,
        slot_permit: OwnedSemaphorePermit,
        mode: RepositoryLeaseMode,
    ) -> Self {
        let connection = Arc::new(tokio::sync::Mutex::new(Some(connection)));
        let slot_permit = Arc::new(tokio::sync::Mutex::new(Some(slot_permit)));
        let monitor_connection = Arc::downgrade(&connection);
        let monitor_slot_permit = Arc::downgrade(&slot_permit);
        let (state_tx, state) = watch::channel(RepositoryLeaseState::Healthy);
        let monitor_state_tx = state_tx.clone();
        let liveness_task = tokio::spawn(async move {
            let mut consecutive_failures = 0_u32;
            loop {
                tokio::time::sleep(REPOSITORY_LEASE_HEARTBEAT_INTERVAL).await;
                let Some(connection_owner) = monitor_connection.upgrade() else {
                    break;
                };
                let result = {
                    let mut connection = connection_owner.lock().await;
                    let Some(connection) = connection.as_mut() else {
                        break;
                    };
                    tokio::time::timeout(
                        REPOSITORY_LEASE_HEARTBEAT_TIMEOUT,
                        sqlx::query("SELECT 1").execute(&mut **connection),
                    )
                    .await
                };
                let failure = match result {
                    Ok(Ok(_)) => {
                        consecutive_failures = 0;
                        None
                    }
                    Ok(Err(error)) => Some((
                        repository_heartbeat_error_is_definitive(&error),
                        error.to_string(),
                    )),
                    Err(_) => Some((
                        false,
                        format!(
                            "heartbeat timed out after {}s",
                            REPOSITORY_LEASE_HEARTBEAT_TIMEOUT.as_secs()
                        ),
                    )),
                };
                if let Some((definitive, error)) = failure {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if !definitive
                        && !repository_heartbeat_failure_is_terminal(consecutive_failures)
                    {
                        tracing::warn!(
                            consecutive_failures,
                            failure_limit = REPOSITORY_LEASE_HEARTBEAT_FAILURE_LIMIT,
                            "PostgreSQL repository advisory-lock heartbeat failed; retaining the session pending confirmation: {error}"
                        );
                        continue;
                    }
                    if definitive {
                        tracing::error!(
                            consecutive_failures,
                            "PostgreSQL repository advisory-lock session was definitively lost: {error}"
                        );
                    } else {
                        tracing::error!(
                            consecutive_failures,
                            "PostgreSQL repository advisory-lock session was lost after repeated heartbeat failures: {error}"
                        );
                    }
                    let started_revoking = transition_repository_lease_state(
                        &monitor_state_tx,
                        RepositoryLeaseState::Healthy,
                        RepositoryLeaseState::Revoking,
                    );
                    if !started_revoking {
                        break;
                    }
                    if let Some(connection_owner) = monitor_connection.upgrade() {
                        if let Some(connection) = connection_owner.lock().await.take() {
                            match tokio::time::timeout(
                                REPOSITORY_LEASE_HEARTBEAT_TIMEOUT,
                                connection.close(),
                            )
                            .await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(close_error)) => tracing::warn!(
                                    "failed to close lost repository advisory-lock session: {close_error}"
                                ),
                                Err(_) => tracing::warn!(
                                    "timed out closing lost repository advisory-lock session"
                                ),
                            }
                        }
                    }
                    if let Some(slot_permit_owner) = monitor_slot_permit.upgrade() {
                        slot_permit_owner.lock().await.take();
                    }
                    if !transition_repository_lease_state(
                        &monitor_state_tx,
                        RepositoryLeaseState::Revoking,
                        RepositoryLeaseState::Lost,
                    ) {
                        tracing::debug!(
                            "repository advisory-lock state changed before loss was reported"
                        );
                    }
                    break;
                }
            }
        });
        Self {
            _connection: connection,
            _slot_permit: slot_permit,
            mode,
            state_tx,
            state,
            liveness_task,
        }
    }

    pub(crate) fn loss_receiver(&self) -> watch::Receiver<RepositoryLeaseState> {
        self.state.clone()
    }

    pub(crate) fn is_healthy(&self) -> bool {
        *self.state.borrow() == RepositoryLeaseState::Healthy
    }

    pub(crate) fn mode(&self) -> RepositoryLeaseMode {
        self.mode
    }

    #[cfg(test)]
    async fn backend_process_id(&self) -> anyhow::Result<i32> {
        let mut connection = self._connection.lock().await;
        let connection = connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("repository advisory-lock session is closed"))?;
        Ok(sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut **connection)
            .await?)
    }
}

impl Drop for RepositoryWriteLease {
    fn drop(&mut self) {
        let released = transition_repository_lease_state(
            &self.state_tx,
            RepositoryLeaseState::Healthy,
            RepositoryLeaseState::Released,
        );
        if released || *self.state.borrow() != RepositoryLeaseState::Revoking {
            self.liveness_task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocation_state_cannot_be_overwritten_by_release() {
        let (sender, receiver) = watch::channel(RepositoryLeaseState::Healthy);
        assert!(transition_repository_lease_state(
            &sender,
            RepositoryLeaseState::Healthy,
            RepositoryLeaseState::Revoking,
        ));
        assert!(!transition_repository_lease_state(
            &sender,
            RepositoryLeaseState::Healthy,
            RepositoryLeaseState::Released,
        ));
        assert!(transition_repository_lease_state(
            &sender,
            RepositoryLeaseState::Revoking,
            RepositoryLeaseState::Lost,
        ));
        assert_eq!(*receiver.borrow(), RepositoryLeaseState::Lost);
    }

    #[test]
    fn healthy_release_prevents_later_revocation() {
        let (sender, receiver) = watch::channel(RepositoryLeaseState::Healthy);
        assert!(transition_repository_lease_state(
            &sender,
            RepositoryLeaseState::Healthy,
            RepositoryLeaseState::Released,
        ));
        assert!(!transition_repository_lease_state(
            &sender,
            RepositoryLeaseState::Healthy,
            RepositoryLeaseState::Revoking,
        ));
        assert_eq!(*receiver.borrow(), RepositoryLeaseState::Released);
    }

    #[test]
    fn repository_heartbeat_requires_repeated_failures_before_revocation() {
        assert!(!repository_heartbeat_failure_is_terminal(1));
        assert!(!repository_heartbeat_failure_is_terminal(2));
        assert!(repository_heartbeat_failure_is_terminal(3));
    }

    #[test]
    fn repository_heartbeat_revokes_immediately_on_definitive_session_errors() {
        assert!(repository_heartbeat_error_is_definitive(
            &sqlx::Error::PoolClosed
        ));
        assert!(repository_heartbeat_error_is_definitive(&sqlx::Error::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset",)
        )));
        assert!(!repository_heartbeat_error_is_definitive(
            &sqlx::Error::RowNotFound
        ));
        assert!(repository_heartbeat_severity_is_definitive(
            sqlx::postgres::PgSeverity::Fatal
        ));
        assert!(!repository_heartbeat_severity_is_definitive(
            sqlx::postgres::PgSeverity::Error
        ));
    }
}

impl WorkspaceLeaseStore {
    #[cfg(test)]
    pub(crate) fn repository_lock_pool_capacity(&self) -> u32 {
        self.repository_lock_pool.options().get_max_connections()
    }

    #[cfg(test)]
    pub(crate) async fn try_acquire_repository_write_lease(
        &self,
        project_key: &str,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        self.acquire_repository_lease_with_timeout(
            project_key,
            RepositoryLeaseMode::Exclusive,
            std::time::Duration::from_secs(10),
            false,
        )
        .await
    }

    pub(crate) async fn acquire_queued_repository_write_lease(
        &self,
        project_key: &str,
    ) -> anyhow::Result<RepositoryWriteLease> {
        self.acquire_repository_lease_with_timeout(
            project_key,
            RepositoryLeaseMode::Exclusive,
            std::time::Duration::from_secs(10),
            true,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("queued exclusive repository lease was not acquired"))
    }

    pub(crate) async fn try_acquire_repository_write_lease_now(
        &self,
        project_key: &str,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        let Ok(slot_permit) = self.repository_lock_slots.clone().try_acquire_owned() else {
            return Ok(None);
        };
        let mut connection = self.repository_lock_pool.acquire().await?;
        connection.close_on_drop();
        let acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
                .bind(project_key)
                .fetch_one(&mut *connection)
                .await?;
        if !acquired {
            return Ok(None);
        }
        Ok(Some(RepositoryWriteLease::monitored(
            connection,
            slot_permit,
            RepositoryLeaseMode::Exclusive,
        )))
    }

    #[cfg(test)]
    pub(crate) async fn try_acquire_repository_shared_lease(
        &self,
        project_key: &str,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        self.acquire_repository_lease_with_timeout(
            project_key,
            RepositoryLeaseMode::Shared,
            std::time::Duration::from_secs(10),
            false,
        )
        .await
    }

    pub(crate) async fn acquire_queued_repository_shared_lease(
        &self,
        project_key: &str,
    ) -> anyhow::Result<RepositoryWriteLease> {
        self.acquire_repository_lease_with_timeout(
            project_key,
            RepositoryLeaseMode::Shared,
            std::time::Duration::from_secs(10),
            true,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("queued shared repository lease was not acquired"))
    }

    async fn acquire_repository_lease_with_timeout(
        &self,
        project_key: &str,
        mode: RepositoryLeaseMode,
        acquire_timeout: std::time::Duration,
        queued: bool,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        let slot_permit = self
            .repository_lock_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("repository advisory-lock slot semaphore closed"))?;
        let mut connection = match tokio::time::timeout(
            acquire_timeout,
            self.repository_lock_pool.acquire(),
        )
        .await
        {
            Ok(Ok(connection)) => connection,
            Err(_) | Ok(Err(sqlx::Error::PoolTimedOut)) => {
                anyhow::bail!(
                    "timed out acquiring a PostgreSQL repository advisory-lock connection"
                );
            }
            Ok(Err(error)) => return Err(error.into()),
        };
        connection.close_on_drop();
        let acquired = match (mode, queued) {
            (RepositoryLeaseMode::Shared, true) => {
                sqlx::query("SELECT pg_advisory_lock_shared(hashtextextended($1, 0))")
                    .bind(project_key)
                    .execute(&mut *connection)
                    .await?;
                true
            }
            (RepositoryLeaseMode::Exclusive, true) => {
                sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
                    .bind(project_key)
                    .execute(&mut *connection)
                    .await?;
                true
            }
            (RepositoryLeaseMode::Shared, false) => {
                sqlx::query_scalar("SELECT pg_try_advisory_lock_shared(hashtextextended($1, 0))")
                    .bind(project_key)
                    .fetch_one(&mut *connection)
                    .await?
            }
            (RepositoryLeaseMode::Exclusive, false) => {
                sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
                    .bind(project_key)
                    .fetch_one(&mut *connection)
                    .await?
            }
        };
        if !acquired {
            return Ok(None);
        }
        Ok(Some(RepositoryWriteLease::monitored(
            connection,
            slot_permit,
            mode,
        )))
    }

    #[cfg(test)]
    pub(crate) fn for_repository_lock_pool_test(repository_lock_pool: PgPool) -> Self {
        let repository_lock_slots = Arc::new(Semaphore::new(
            repository_lock_pool.options().get_max_connections() as usize,
        ));
        Self {
            pool: repository_lock_pool.clone(),
            repository_lock_pool,
            repository_lock_slots,
            store_key: "repository-lock-pool-test".to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn try_acquire_repository_write_lease_for_test(
        &self,
        project_key: &str,
        acquire_timeout: std::time::Duration,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        self.acquire_repository_lease_with_timeout(
            project_key,
            RepositoryLeaseMode::Exclusive,
            acquire_timeout,
            false,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn terminate_repository_lease_for_test(
        &self,
        lease: &RepositoryWriteLease,
    ) -> anyhow::Result<bool> {
        let process_id = lease.backend_process_id().await?;
        Ok(sqlx::query_scalar("SELECT pg_terminate_backend($1)")
            .bind(process_id)
            .fetch_one(&self.pool)
            .await?)
    }

    #[cfg(test)]
    pub(crate) async fn queued_repository_lock_waiter_count_for_test(
        &self,
        project_key: &str,
    ) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(
            "WITH lock_key AS (SELECT hashtextextended($1, 0) AS value)
             SELECT COUNT(*)
             FROM pg_locks, lock_key
             WHERE locktype = 'advisory'
               AND NOT granted
               AND classid = ((value >> 32) & 4294967295)::oid
               AND objid = (value & 4294967295)::oid
               AND objsubid = 1",
        )
        .bind(project_key)
        .fetch_one(&self.pool)
        .await?)
    }
}
