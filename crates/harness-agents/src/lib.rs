pub mod anthropic_api;
pub mod builder;
pub mod claude;
pub mod claude_adapter;
mod claude_stream;
mod cloud_setup;
pub mod codex;
pub mod codex_adapter;
pub mod compress_model;
pub mod provider_backpressure;
pub mod registry;
pub mod scoped_token;
mod spawn_contract;
mod streaming;

use harness_core::run_id::{RunIdentity, AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};
use harness_core::run_registry::{append_binding_nonblocking, BindingRecord};
use std::collections::HashMap;
use std::path::Path;

pub(crate) fn resolve_agent_run_identity(env_vars: &HashMap<String, String>) -> RunIdentity {
    match RunIdentity::from_env_vars(env_vars) {
        Ok(Some(identity)) => identity,
        Ok(None) => RunIdentity::mint(),
        Err(error) => {
            tracing::error!(
                "invalid agent run identity environment; minting a new run id: {error}"
            );
            RunIdentity::mint()
        }
    }
}

pub(crate) fn apply_agent_run_identity_env(
    cmd: &mut tokio::process::Command,
    identity: &RunIdentity,
) {
    cmd.env(AGENT_RUN_ID_ENV, identity.run_id.as_str());
    if let Some(parent) = &identity.parent {
        cmd.env(AGENT_RUN_PARENT_ENV, parent.as_str());
    } else {
        cmd.env_remove(AGENT_RUN_PARENT_ENV);
    }
}

pub(crate) fn classify_missing_workspace_spawn_failure(
    error: &std::io::Error,
    project_root: &Path,
    fallback_message: String,
) -> String {
    if error.kind() == std::io::ErrorKind::NotFound
        && matches!(project_root.try_exists(), Ok(false))
    {
        format!(
            "workspace missing: {}; {fallback_message}",
            project_root.display()
        )
    } else {
        fallback_message
    }
}

pub(crate) fn write_provisional_agent_run_binding(
    identity: &RunIdentity,
    native_kind: &str,
    pid: u32,
    cwd: &Path,
) {
    let record = provisional_agent_run_binding_record(identity, native_kind, pid, cwd);
    append_binding_nonblocking(&record);
}

pub(crate) fn provisional_agent_run_binding_record(
    identity: &RunIdentity,
    native_kind: &str,
    pid: u32,
    cwd: &Path,
) -> BindingRecord {
    BindingRecord::provisional(
        identity.run_id.clone(),
        identity.parent.clone(),
        native_kind,
        pid,
        cwd.to_path_buf(),
        "harness-adapter",
    )
}

/// Place the child process into its own process group.
///
/// Uses the stable `CommandExt::process_group(0)` API (Rust 1.64+).
/// When the child is later killed, we can send `SIGKILL` to the entire
/// process group to also terminate grandchild processes like `cargo test`
/// binaries.
#[cfg(unix)]
pub(crate) fn set_process_group(cmd: &mut tokio::process::Command) {
    cmd.process_group(0);
}

#[cfg(unix)]
fn kill_process_group_id(pid: u32) {
    // kill(-pgid, SIGKILL) kills the entire process group.
    // SAFETY: standard POSIX signal, no memory unsafety.
    let ret = unsafe { nix_kill(-(pid as i32), 9) };
    if ret == 0 {
        tracing::debug!(pgid = pid, "killed process group");
    } else {
        tracing::warn!(pgid = pid, "failed to kill process group");
    }
}

/// Kill the entire process group rooted at `child`.
///
/// Sends `SIGKILL` to `-pid` (the process group) so that all descendants
/// (cargo test binaries, shell subprocesses, etc.) are terminated together.
#[cfg(unix)]
pub(crate) fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        kill_process_group_id(pid);
    }
}

#[cfg(unix)]
fn process_group_has_members(pid: u32) -> bool {
    // kill(-pgid, 0) performs existence/permission checking without sending a
    // signal. A non-zero result is treated as drained; in this use case Harness
    // owns the child group, so EPERM should not hide live descendants.
    (unsafe { nix_kill(-(pid as i32), 0) }) == 0
}

/// Raw kill(2) syscall without libc dependency.
#[cfg(unix)]
unsafe fn nix_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

pub(crate) struct ManagedChild {
    /// Only `None` after `Drop` has taken the child for background reaping;
    /// every other method may assume it is present.
    child: Option<tokio::process::Child>,
    process_group_id: Option<u32>,
    label: &'static str,
    cleanup_disarmed: bool,
}

impl ManagedChild {
    pub(crate) fn new(child: tokio::process::Child, label: &'static str) -> Self {
        let process_group_id = child.id();
        Self {
            child: Some(child),
            process_group_id,
            label,
            cleanup_disarmed: false,
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("ManagedChild is only vacated during drop")
    }

    pub(crate) fn inner_mut(&mut self) -> &mut tokio::process::Child {
        self.child_mut()
    }

    pub(crate) fn terminate_now(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process_group_id {
            kill_process_group_id(pid);
        }
        let _ = self.child_mut().start_kill();
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child_mut().wait().await
    }

    pub(crate) async fn wait_and_cleanup_descendants(
        &mut self,
    ) -> std::io::Result<std::process::ExitStatus> {
        let status = self.wait().await?;
        self.cleanup_after_child_exit().await?;
        Ok(status)
    }

    pub(crate) async fn cleanup_after_child_exit(&mut self) -> std::io::Result<()> {
        self.kill_descendants_after_child_exit().await?;
        self.cleanup_disarmed = true;
        Ok(())
    }

    pub(crate) async fn wait_with_output(&mut self) -> std::io::Result<std::process::Output> {
        use tokio::io::AsyncReadExt;

        let stdout = self.child_mut().stdout.take();
        let stderr = self.child_mut().stderr.take();
        let stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = stdout {
                pipe.read_to_end(&mut bytes).await?;
            }
            Ok::<_, std::io::Error>(bytes)
        });
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = stderr {
                pipe.read_to_end(&mut bytes).await?;
            }
            Ok::<_, std::io::Error>(bytes)
        });

        let status = self.wait_and_cleanup_descendants().await?;
        let stdout = join_reader(stdout_task).await?;
        let stderr = join_reader(stderr_task).await?;

        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    }

    #[cfg(unix)]
    async fn kill_descendants_after_child_exit(&mut self) -> std::io::Result<()> {
        let Some(process_group_id) = self.process_group_id else {
            return Ok(());
        };
        if !process_group_has_members(process_group_id) {
            return Ok(());
        }

        tracing::warn!(
            agent_process = self.label,
            pgid = process_group_id,
            "agent root exited while descendants remained; killing process group before workspace release"
        );
        kill_process_group_id(process_group_id);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !process_group_has_members(process_group_id) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for agent process group {process_group_id} to drain"
                    ),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[cfg(not(unix))]
    async fn kill_descendants_after_child_exit(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<Vec<u8>> {
    task.await
        .map_err(|error| std::io::Error::other(error.to_string()))?
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.cleanup_disarmed {
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        let child_reaped = match child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    agent_process = self.label,
                    "failed to inspect child process before drop: {error}"
                );
                true
            }
        };

        #[cfg(unix)]
        let group_has_members = self.process_group_id.is_some_and(process_group_has_members);
        #[cfg(not(unix))]
        let group_has_members = false;

        if child_reaped && !group_has_members {
            self.cleanup_disarmed = true;
            return;
        }

        tracing::warn!(
            agent_process = self.label,
            "agent child dropped while still running; killing process group before workspace release"
        );
        #[cfg(unix)]
        if let Some(pid) = self.process_group_id {
            kill_process_group_id(pid);
        }
        let _ = child.start_kill();

        // Drop runs on the async runtime for every cancelled/timed-out turn, so
        // it must not block the worker thread: hand reaping and group-drain
        // verification to a detached task. The blocking loop is kept only for
        // drops outside a runtime (e.g. process teardown).
        let label = self.label;
        let process_group_id = self.process_group_id;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(reap_killed_child(child, label, process_group_id));
            }
            Err(_) => drain_killed_child_blocking(child, child_reaped, label, process_group_id),
        }
    }
}

/// Await the killed child's exit and verify its process group drains.
///
/// The SIGKILL was already issued by `Drop`; this task only reaps and reports.
async fn reap_killed_child(
    mut child: tokio::process::Child,
    label: &'static str,
    process_group_id: Option<u32>,
) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                agent_process = label,
                "failed waiting for killed agent child to exit: {error}"
            );
        }
        Err(_) => {
            tracing::warn!(
                agent_process = label,
                "timed out waiting for killed agent child to exit"
            );
            return;
        }
    }

    #[cfg(unix)]
    if let Some(pgid) = process_group_id {
        loop {
            if !process_group_has_members(pgid) {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    agent_process = label,
                    "timed out waiting for killed agent process group to drain"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    #[cfg(not(unix))]
    let _ = process_group_id;
}

/// Blocking fallback for drops outside a tokio runtime, where a detached
/// reaper task cannot be spawned.
fn drain_killed_child_blocking(
    mut child: tokio::process::Child,
    mut child_reaped: bool,
    label: &'static str,
    process_group_id: Option<u32>,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if !child_reaped {
            match child.try_wait() {
                Ok(Some(_)) => {
                    child_reaped = true;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        agent_process = label,
                        "failed waiting for killed agent child to exit: {error}"
                    );
                    child_reaped = true;
                }
            }
        }

        #[cfg(unix)]
        let group_drained = process_group_id.is_none_or(|pid| !process_group_has_members(pid));
        #[cfg(not(unix))]
        let group_drained = true;

        if child_reaped && group_drained {
            return;
        }

        if std::time::Instant::now() >= deadline {
            if !child_reaped {
                tracing::warn!(
                    agent_process = label,
                    "timed out waiting for killed agent child to exit"
                );
            }
            if !group_drained {
                tracing::warn!(
                    agent_process = label,
                    "timed out waiting for killed agent process group to drain"
                );
            }
            return;
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
#[cfg(unix)]
mod managed_child_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_of_running_child_returns_promptly_and_reaps_in_background() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 5").kill_on_drop(true);
        set_process_group(&mut cmd);
        let child = cmd.spawn().expect("spawn sleeping child");
        let pgid = child.id().expect("child pid");
        let managed = ManagedChild::new(child, "drop latency test");

        let start = std::time::Instant::now();
        drop(managed);
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "drop must not block the runtime worker; took {elapsed:?}"
        );

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !process_group_has_members(pgid) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "detached reaper should drain the killed process group"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn drop_outside_runtime_falls_back_to_blocking_drain() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let (managed, pgid) = runtime.block_on(async {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg("-c").arg("sleep 5").kill_on_drop(true);
            set_process_group(&mut cmd);
            let child = cmd.spawn().expect("spawn sleeping child");
            let pgid = child.id().expect("child pid");
            (ManagedChild::new(child, "blocking drain test"), pgid)
        });

        // Dropping outside any runtime context must still fully drain the
        // group before returning (there is no executor to run a reaper task).
        drop(managed);
        assert!(
            !process_group_has_members(pgid),
            "blocking fallback should drain the killed process group before returning"
        );
        drop(runtime);
    }
}

#[cfg(test)]
mod run_id_tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn run_id_resolution_prefers_request_env() {
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_RUN_ID_ENV.to_string(),
            "ar-01j1qb3c9r7v5m2k8x4tznq6wd".to_string(),
        );
        env_vars.insert(
            AGENT_RUN_PARENT_ENV.to_string(),
            "ar-01j1qb3c9r7v5m2k8x4tznq6we".to_string(),
        );

        let identity = resolve_agent_run_identity(&env_vars);

        assert_eq!(identity.run_id.as_str(), "ar-01j1qb3c9r7v5m2k8x4tznq6wd");
        assert_eq!(
            identity.parent.as_ref().map(|id| id.as_str()),
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6we")
        );
    }

    #[test]
    fn run_id_resolution_mints_when_absent() {
        let _guard = env_lock().lock().unwrap();
        let original_id = std::env::var(AGENT_RUN_ID_ENV).ok();
        let original_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
        unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) };
        unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) };

        let identity = resolve_agent_run_identity(&HashMap::new());

        assert!(identity.run_id.as_str().starts_with("ar-"));
        assert!(identity.parent.is_none());

        match original_id {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_ID_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) },
        }
        match original_parent {
            Some(value) => unsafe { std::env::set_var(AGENT_RUN_PARENT_ENV, value) },
            None => unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) },
        }
    }

    #[test]
    fn run_id_provisional_binding_uses_harness_adapter_source() {
        let identity = RunIdentity::from_env_values(
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6wd"),
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6we"),
        )
        .expect("valid identity")
        .expect("identity");

        let record =
            provisional_agent_run_binding_record(&identity, "claude-code", 42, Path::new("/tmp/x"));

        assert_eq!(record.run_id.as_str(), "ar-01j1qb3c9r7v5m2k8x4tznq6wd");
        assert_eq!(
            record.parent.as_ref().map(|id| id.as_str()),
            Some("ar-01j1qb3c9r7v5m2k8x4tznq6we")
        );
        assert_eq!(record.native.kind, "claude-code");
        assert!(record.native.id.is_empty());
        assert_eq!(record.pid, 42);
        assert_eq!(record.source, "harness-adapter");
    }
}

#[cfg(test)]
mod spawn_failure_tests {
    use super::*;

    #[test]
    fn missing_workspace_spawn_failure_is_primary_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("missing-workspace");
        let error = std::io::Error::from_raw_os_error(2);

        let message = classify_missing_workspace_spawn_failure(
            &error,
            &missing,
            "failed to run codex: No such file or directory".to_string(),
        );

        assert!(
            message.starts_with(&format!("workspace missing: {}", missing.display())),
            "missing workspace must be the primary error, got: {message}"
        );
        assert!(message.contains("failed to run codex"));
        Ok(())
    }

    #[test]
    fn missing_binary_in_existing_workspace_keeps_original_error(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let error = std::io::Error::from_raw_os_error(2);

        let message = classify_missing_workspace_spawn_failure(
            &error,
            dir.path(),
            "failed to run codex: No such file or directory".to_string(),
        );

        assert_eq!(message, "failed to run codex: No such file or directory");
        Ok(())
    }
}
