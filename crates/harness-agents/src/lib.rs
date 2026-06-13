pub mod anthropic_api;
pub mod claude;
pub mod claude_adapter;
mod claude_stream;
mod cloud_setup;
pub mod codex;
pub mod codex_adapter;
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
    label: &'static str,
    reaped: bool,
}

impl ManagedChild {
    pub(crate) fn new(child: tokio::process::Child, label: &'static str) -> Self {
        Self {
            child,
            label,
            reaped: false,
        }
    }

    pub(crate) fn inner_mut(&mut self) -> &mut tokio::process::Child {
        &mut self.child
    }

    pub(crate) fn terminate_now(&mut self) {
        #[cfg(unix)]
        kill_process_group(&self.child);
        let _ = self.child.start_kill();
    }

    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        self.reaped = true;
        Ok(status)
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

        let status = self.wait().await?;
        let stdout = join_reader(stdout_task).await?;
        let stderr = join_reader(stderr_task).await?;

        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
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
        if self.reaped {
            return;
        }
        match self.child.try_wait() {
            Ok(Some(_)) => {
                self.reaped = true;
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    agent_process = self.label,
                    "failed to inspect child process before drop: {error}"
                );
            }
        }

        tracing::warn!(
            agent_process = self.label,
            "agent child dropped while still running; killing process group before workspace release"
        );
        let process_group_id = self.child.id();
        self.terminate_now();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut child_reaped = false;
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
            let group_drained = process_group_id.is_none_or(|pid| !process_group_has_members(pid));
            #[cfg(not(unix))]
            let group_drained = true;

            if child_reaped && group_drained {
                self.reaped = true;
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
