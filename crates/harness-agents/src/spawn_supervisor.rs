use crate::spawn_contract::PreparedAgentSpawn;
use crate::ManagedChild;
use harness_core::capability::CapabilityToken;
use harness_core::error::HarnessError;
use harness_core::run_id::RunIdentity;
use std::process::Stdio;
use tokio::process::Command;

const ETXTBSY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

pub(crate) struct AgentStdio {
    pub(crate) stdin: Stdio,
    pub(crate) stdout: Stdio,
    pub(crate) stderr: Stdio,
}

impl AgentStdio {
    pub(crate) fn piped_output(stdin: Stdio) -> Self {
        Self {
            stdin,
            stdout: Stdio::piped(),
            stderr: Stdio::piped(),
        }
    }
}

pub(crate) type SpawnErrorMapper<'a> =
    Box<dyn Fn(&std::io::Error, &PreparedAgentSpawn) -> HarnessError + Send + 'a>;

pub(crate) struct AgentSpawnPlan<'a> {
    pub(crate) prepared_spawn: PreparedAgentSpawn,
    pub(crate) run_identity: RunIdentity,
    pub(crate) native_kind: &'static str,
    pub(crate) process_label: &'static str,
    pub(crate) stdio: AgentStdio,
    pub(crate) extra_env_removals: Vec<String>,
    pub(crate) map_spawn_error: SpawnErrorMapper<'a>,
}

pub(crate) struct SupervisedAgentProcess {
    pub(crate) prepared_spawn: PreparedAgentSpawn,
    pub(crate) child: ManagedChild,
}

pub(crate) async fn spawn_agent(
    plan: AgentSpawnPlan<'_>,
    capability_token: Option<&CapabilityToken>,
) -> harness_core::error::Result<SupervisedAgentProcess> {
    let AgentSpawnPlan {
        prepared_spawn,
        run_identity,
        native_kind,
        process_label,
        stdio,
        extra_env_removals,
        map_spawn_error,
    } = plan;

    let mut cmd = Command::new(&prepared_spawn.program);
    cmd.args(&prepared_spawn.args)
        .current_dir(&prepared_spawn.current_dir)
        .stdin(stdio.stdin)
        .stdout(stdio.stdout)
        .stderr(stdio.stderr)
        .kill_on_drop(true);
    #[cfg(unix)]
    crate::set_process_group(&mut cmd);
    crate::spawn_contract::apply_process_env(&mut cmd, &prepared_spawn);
    for key in &extra_env_removals {
        cmd.env_remove(key);
    }

    let child = match spawn_with_etxtbsy_retry(
        || validate_capability_token(capability_token),
        || cmd.spawn(),
    )
    .await
    {
        Ok(child) => child,
        Err(SpawnAttemptError::Authorization(error)) => return Err(error),
        Err(SpawnAttemptError::Io(error)) => {
            let mapped = (map_spawn_error)(&error, &prepared_spawn);
            tracing::error!(
                agent = native_kind,
                process = process_label,
                error_kind = ?error.kind(),
                "{mapped}"
            );
            return Err(mapped);
        }
    };

    if let Some(pid) = child.id() {
        crate::write_provisional_agent_run_binding(
            &run_identity,
            native_kind,
            pid,
            &prepared_spawn.current_dir,
        );
    }

    let managed_child = ManagedChild::new(child, process_label)
        .with_egress_proxy_lease(prepared_spawn.egress_proxy_lease.clone());
    Ok(SupervisedAgentProcess {
        prepared_spawn,
        child: managed_child,
    })
}

pub(crate) fn validate_capability_token(
    capability_token: Option<&CapabilityToken>,
) -> harness_core::error::Result<()> {
    if let Some(token) = capability_token {
        if token.is_expired() {
            return Err(HarnessError::AgentExecution(format!(
                "capability token for subtask {} has expired",
                token.subtask_index
            )));
        }
    }
    Ok(())
}

#[derive(Debug)]
enum SpawnAttemptError {
    Authorization(HarnessError),
    Io(std::io::Error),
}

fn authorized_spawn<A, F, T>(authorize: &mut A, spawn: &mut F) -> Result<T, SpawnAttemptError>
where
    A: FnMut() -> harness_core::error::Result<()>,
    F: FnMut() -> std::io::Result<T>,
{
    authorize().map_err(SpawnAttemptError::Authorization)?;
    spawn().map_err(SpawnAttemptError::Io)
}

async fn spawn_with_etxtbsy_retry<A, F, T>(
    mut authorize: A,
    mut spawn: F,
) -> Result<T, SpawnAttemptError>
where
    A: FnMut() -> harness_core::error::Result<()>,
    F: FnMut() -> std::io::Result<T>,
{
    match authorized_spawn(&mut authorize, &mut spawn) {
        Err(SpawnAttemptError::Io(error)) if is_etxtbsy(&error) => {
            tokio::time::sleep(ETXTBSY_RETRY_DELAY).await;
            authorized_spawn(&mut authorize, &mut spawn)
        }
        result => result,
    }
}

fn is_etxtbsy(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(26)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn spawn_retry_retries_once_for_etxtbsy() {
        let attempts = AtomicUsize::new(0);

        let result = spawn_with_etxtbsy_retry(
            || Ok(()),
            || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(std::io::Error::from_raw_os_error(26))
                } else {
                    Ok("spawned")
                }
            },
        )
        .await
        .expect("ETXTBSY retry should use the second spawn result");

        assert_eq!(result, "spawned");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn spawn_retry_does_not_retry_other_errors() {
        let attempts = AtomicUsize::new(0);

        let error = spawn_with_etxtbsy_retry(
            || Ok(()),
            || -> std::io::Result<()> {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::from_raw_os_error(2))
            },
        )
        .await
        .expect_err("non-ETXTBSY errors must not be retried");

        assert!(matches!(
            error,
            SpawnAttemptError::Io(error) if error.raw_os_error() == Some(2)
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn token_expiring_during_etxtbsy_delay_blocks_second_spawn() {
        let attempts = AtomicUsize::new(0);
        let authorization_checks = AtomicUsize::new(0);
        let mut token = CapabilityToken::new(9, Vec::new(), std::time::Duration::from_secs(60));

        let error = spawn_with_etxtbsy_retry(
            || {
                if authorization_checks.fetch_add(1, Ordering::SeqCst) == 1 {
                    token.expires_at = std::time::SystemTime::UNIX_EPOCH;
                }
                validate_capability_token(Some(&token))
            },
            || -> std::io::Result<()> {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::from_raw_os_error(26))
            },
        )
        .await
        .expect_err("expiry during retry delay must block the second spawn attempt");

        assert!(matches!(
            error,
            SpawnAttemptError::Authorization(HarnessError::AgentExecution(message))
                if message.contains("subtask 9 has expired")
        ));
        assert_eq!(authorization_checks.load(Ordering::SeqCst), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
