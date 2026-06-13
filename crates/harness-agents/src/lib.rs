pub mod anthropic_api;
pub mod claude;
pub mod claude_adapter;
mod claude_stream;
mod cloud_setup;
pub mod codex;
pub mod codex_adapter;
pub mod provider_backpressure;
pub mod registry;
mod streaming;

/// Remove all `CLAUDE`-prefixed environment variables from a command to prevent
/// nested Claude Code detection (SIGTRAP).
pub(crate) fn strip_claude_env(cmd: &mut tokio::process::Command) {
    let claude_keys: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE"))
        .map(|(k, _)| k)
        .collect();
    for key in &claude_keys {
        cmd.env_remove(key);
    }
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
    child: tokio::process::Child,
    process_group_id: Option<u32>,
    label: &'static str,
    cleanup_disarmed: bool,
}

impl ManagedChild {
    pub(crate) fn new(child: tokio::process::Child, label: &'static str) -> Self {
        let process_group_id = child.id();
        Self {
            child,
            process_group_id,
            label,
            cleanup_disarmed: false,
        }
    }

    pub(crate) fn inner_mut(&mut self) -> &mut tokio::process::Child {
        &mut self.child
    }

    pub(crate) fn terminate_now(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process_group_id {
            kill_process_group_id(pid);
        }
        let _ = self.child.start_kill();
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
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

        let stdout = self.child.stdout.take();
        let stderr = self.child.stderr.take();
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
        let mut child_reaped = match self.child.try_wait() {
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
        self.terminate_now();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !child_reaped {
                match self.child.try_wait() {
                    Ok(Some(_)) => {
                        child_reaped = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            agent_process = self.label,
                            "failed waiting for killed agent child to exit: {error}"
                        );
                        child_reaped = true;
                    }
                }
            }

            #[cfg(unix)]
            let group_drained = self
                .process_group_id
                .is_none_or(|pid| !process_group_has_members(pid));
            #[cfg(not(unix))]
            let group_drained = true;

            if child_reaped && group_drained {
                self.cleanup_disarmed = true;
                return;
            }

            if std::time::Instant::now() >= deadline {
                if !child_reaped {
                    tracing::warn!(
                        agent_process = self.label,
                        "timed out waiting for killed agent child to exit"
                    );
                }
                if !group_drained {
                    tracing::warn!(
                        agent_process = self.label,
                        "timed out waiting for killed agent process group to drain"
                    );
                }
                return;
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
