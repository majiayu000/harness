pub mod anthropic_api;
pub mod builder;
pub mod claude;
mod claude_stream;
pub mod claude_stream_json;
mod cloud_setup;
pub mod codex;
pub mod codex_adapter;
pub mod compress_model;
pub mod docker_reconciliation;
pub mod opencode;
pub mod opencode_adapter;
mod output_capture;
pub mod provider_backpressure;
pub mod registry;
pub mod runtime_fingerprint;
pub mod scoped_token;
mod spawn_contract;
mod spawn_supervisor;
mod streaming;

use harness_core::run_id::RunIdentity;
#[cfg(test)]
use harness_core::run_id::{AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};
use harness_core::run_registry::{append_binding_nonblocking, BindingRecord};
use output_capture::OutputCapture;
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
    egress_proxy_lease: Option<std::sync::Arc<crate::spawn_contract::egress::EgressProxyLease>>,
    egress_verification: crate::spawn_contract::EgressVerification,
}

impl ManagedChild {
    pub(crate) fn new(child: tokio::process::Child, label: &'static str) -> Self {
        let process_group_id = child.id();
        Self {
            child: Some(child),
            process_group_id,
            label,
            cleanup_disarmed: false,
            egress_proxy_lease: None,
            egress_verification: crate::spawn_contract::EgressVerification::NotRequired,
        }
    }

    pub(crate) fn with_egress_proxy_lease(
        mut self,
        lease: Option<std::sync::Arc<crate::spawn_contract::egress::EgressProxyLease>>,
    ) -> Self {
        self.egress_proxy_lease = lease;
        self
    }

    pub(crate) async fn validate_egress_proxy(&self) -> harness_core::error::Result<()> {
        let Some(lease) = self.egress_proxy_lease.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || lease.validate_health())
            .await
            .map_err(|error| {
                harness_core::error::HarnessError::AgentExecution(format!(
                    "egress proxy health check task failed: {error}"
                ))
            })?
    }

    pub(crate) fn with_egress_verification(
        mut self,
        verification: crate::spawn_contract::EgressVerification,
    ) -> Self {
        self.egress_verification = verification;
        self
    }

    pub(crate) fn egress_verified_before_spawn(&self) -> bool {
        self.egress_verification == crate::spawn_contract::EgressVerification::VerifiedBeforeSpawn
    }

    pub(crate) fn awaits_container_egress_canary(&self) -> bool {
        self.egress_verification == crate::spawn_contract::EgressVerification::AwaitContainerCanary
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

    /// Wait for the child while capturing bounded output with an idle timeout.
    ///
    /// Unlike `Child::wait_with_output`, this never buffers unbounded process
    /// output (only the tail up to `limits.max_captured_bytes` per stream is
    /// kept, which preserves the trailing result line agents emit) and it
    /// declares the child a zombie when neither pipe produces data for
    /// `limits.idle_timeout`, killing the process group instead of hanging
    /// the caller forever.
    pub(crate) async fn wait_with_output(
        &mut self,
        limits: &OutputLimits,
    ) -> std::io::Result<BoundedOutput> {
        self.wait_with_redacted_output(limits, &[]).await
    }

    /// Wait while redacting configured values before bounded tail capture.
    pub(crate) async fn wait_with_redacted_output(
        &mut self,
        limits: &OutputLimits,
        secret_values: &[String],
    ) -> std::io::Result<BoundedOutput> {
        let mut stdout_pipe = self.child_mut().stdout.take();
        let mut stderr_pipe = self.child_mut().stderr.take();
        let mut stdout_buf = OutputCapture::new(limits.max_captured_bytes, secret_values);
        let mut stderr_buf = OutputCapture::new(limits.max_captured_bytes, secret_values);
        let mut stdout_chunk = vec![0u8; OUTPUT_READ_CHUNK_BYTES];
        let mut stderr_chunk = vec![0u8; OUTPUT_READ_CHUNK_BYTES];

        let mut exit_status: Option<std::process::ExitStatus> = None;
        while stdout_pipe.is_some() || stderr_pipe.is_some() {
            let child_running = exit_status.is_none();
            let read = async {
                tokio::select! {
                    result = read_from_pipe(&mut stdout_pipe, &mut stdout_chunk) => {
                        PipeRead::Stdout(result)
                    }
                    result = read_from_pipe(&mut stderr_pipe, &mut stderr_chunk) => {
                        PipeRead::Stderr(result)
                    }
                    // Watch for root exit while pipes stay open: a descendant
                    // holding the pipe must be killed so the reads reach EOF
                    // instead of hanging until the idle timeout.
                    result = self.child.as_mut().expect(
                        "ManagedChild is only vacated during drop",
                    ).wait(), if child_running => {
                        PipeRead::Exited(result)
                    }
                }
            };
            let event = if let Some(idle) = limits.idle_timeout {
                match tokio::time::timeout(idle, read).await {
                    Ok(event) => event,
                    Err(_) => {
                        self.terminate_now();
                        return Err(self.idle_timeout_error(idle));
                    }
                }
            } else {
                read.await
            };
            match event {
                PipeRead::Stdout(Ok(0)) => stdout_pipe = None,
                PipeRead::Stderr(Ok(0)) => stderr_pipe = None,
                PipeRead::Stdout(Ok(n)) => stdout_buf.push(&stdout_chunk[..n]),
                PipeRead::Stderr(Ok(n)) => stderr_buf.push(&stderr_chunk[..n]),
                PipeRead::Stdout(Err(error)) | PipeRead::Stderr(Err(error)) => {
                    self.terminate_now();
                    return Err(error);
                }
                PipeRead::Exited(result) => {
                    exit_status = Some(result?);
                    self.cleanup_after_child_exit().await?;
                }
            }
        }

        stdout_buf.finish();
        stderr_buf.finish();

        let status = match exit_status {
            Some(status) => status,
            None => {
                if let Some(idle) = limits.idle_timeout {
                    match tokio::time::timeout(idle, self.wait_and_cleanup_descendants()).await {
                        Ok(result) => result?,
                        Err(_) => {
                            self.terminate_now();
                            return Err(self.idle_timeout_error(idle));
                        }
                    }
                } else {
                    self.wait_and_cleanup_descendants().await?
                }
            }
        };

        if stdout_buf.truncated() || stderr_buf.truncated() {
            tracing::warn!(
                agent_process = self.label,
                max_captured_bytes = limits.max_captured_bytes,
                stdout_truncated = stdout_buf.truncated(),
                stderr_truncated = stderr_buf.truncated(),
                "agent output exceeded the capture limit; kept only the tail"
            );
        }

        Ok(BoundedOutput {
            status,
            stdout: stdout_buf.into_data(),
            stderr: stderr_buf.into_data(),
        })
    }

    fn idle_timeout_error(&self, idle: std::time::Duration) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "{} idle timeout after {}s: zombie process terminated",
                self.label,
                idle.as_secs()
            ),
        )
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

/// Default per-stream capture ceiling for non-streaming execution. Generous
/// enough for any legitimate agent transcript while preventing a runaway
/// child (e.g. one that dumps a build log) from holding gigabytes in RAM.
pub(crate) const DEFAULT_MAX_CAPTURED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

const OUTPUT_READ_CHUNK_BYTES: usize = 64 * 1024;

/// Limits applied by [`ManagedChild::wait_with_output`].
pub(crate) struct OutputLimits {
    /// Kill the child when neither pipe produces data for this long.
    pub(crate) idle_timeout: Option<std::time::Duration>,
    /// Per-stream capture ceiling; only the tail is kept beyond it.
    pub(crate) max_captured_bytes: usize,
}

impl OutputLimits {
    /// Build limits from an agent's `stream_timeout_secs` configuration so the
    /// non-streaming path shares the streaming path's zombie detection.
    pub(crate) fn from_stream_timeout_secs(secs: Option<u64>) -> Self {
        Self {
            idle_timeout: secs.map(std::time::Duration::from_secs),
            max_captured_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

enum PipeRead {
    Stdout(std::io::Result<usize>),
    Stderr(std::io::Result<usize>),
    Exited(std::io::Result<std::process::ExitStatus>),
}

/// Read from an optional pipe; a vacated pipe never resolves, letting
/// `select!` wait solely on the remaining stream.
async fn read_from_pipe<R>(pipe: &mut Option<R>, buf: &mut [u8]) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    match pipe {
        Some(reader) => reader.read(buf).await,
        None => std::future::pending().await,
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.cleanup_disarmed {
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        let egress_proxy_lease = self.egress_proxy_lease.take();
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
                handle.spawn(reap_killed_child(
                    child,
                    label,
                    process_group_id,
                    egress_proxy_lease,
                ));
            }
            Err(_) => drain_killed_child_blocking(
                child,
                child_reaped,
                label,
                process_group_id,
                egress_proxy_lease,
            ),
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
    _egress_proxy_lease: Option<std::sync::Arc<crate::spawn_contract::egress::EgressProxyLease>>,
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
    _egress_proxy_lease: Option<std::sync::Arc<crate::spawn_contract::egress::EgressProxyLease>>,
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

    fn spawn_shell(script: &str) -> ManagedChild {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        set_process_group(&mut cmd);
        ManagedChild::new(
            cmd.spawn().expect("spawn shell child"),
            "managed child test",
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_with_output_times_out_on_silent_child() {
        let mut child = spawn_shell("sleep 30");
        let limits = OutputLimits {
            idle_timeout: Some(std::time::Duration::from_millis(300)),
            max_captured_bytes: DEFAULT_MAX_CAPTURED_OUTPUT_BYTES,
        };

        let start = std::time::Instant::now();
        let error = child
            .wait_with_output(&limits)
            .await
            .expect_err("a silent child must trip the idle timeout");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "timeout must fire near the configured deadline, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_with_output_caps_capture_to_the_tail() {
        let mut child = spawn_shell("printf 'a%.0s' $(seq 1 8000); printf 'END-MARKER'; exit 0");
        let limits = OutputLimits {
            idle_timeout: Some(std::time::Duration::from_secs(10)),
            max_captured_bytes: 512,
        };

        let output = child
            .wait_with_output(&limits)
            .await
            .expect("bounded wait should succeed");
        assert!(output.status.success());
        assert!(output.stdout.len() <= 512, "len={}", output.stdout.len());
        assert!(
            output.stdout.ends_with(b"END-MARKER"),
            "the trailing bytes must survive truncation"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_with_redacted_output_masks_before_tail_capture() -> anyhow::Result<()> {
        let secret = "TOP-SECRET-TOKEN".to_string();
        let mut child = spawn_shell("printf 'prefix-TOP-SECRET-TOKEN-tail'");
        let limits = OutputLimits {
            idle_timeout: Some(std::time::Duration::from_secs(10)),
            max_captured_bytes: 12,
        };

        let output = child.wait_with_redacted_output(&limits, &[secret]).await?;

        assert!(output.status.success());
        assert_eq!(output.stdout, b"fix-***-tail");
        Ok(())
    }

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
#[path = "run_id_tests.rs"]
mod run_id_tests;

#[cfg(test)]
mod egress_verification_tests;

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
