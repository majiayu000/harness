//! Process group lifecycle management for agent subprocesses.
//!
//! On Unix, all agent spawns call `setsid()` in `pre_exec`, placing the child
//! process in its own session and process group. [`ProcessGroupGuard`] kills
//! the entire process group on drop, eliminating orphaned grandchild processes
//! that `kill_on_drop(true)` misses (it only kills the direct child).

/// Configure a command to run in a new process group via `setsid()`.
///
/// On Unix: attaches a `pre_exec` hook that calls `setsid()` after fork, so
/// the child becomes its own session and process group leader. Killing the
/// process group (via [`kill_process_group`]) then reaches all grandchild
/// processes spawned by the agent CLI (node, cargo, rustc, etc.).
///
/// # Safety
///
/// The `pre_exec` closure runs in the child process after `fork()` but before
/// `exec()`. Only async-signal-safe functions may be called. `setsid()` is
/// specified as async-signal-safe by POSIX.
#[cfg(unix)]
pub(crate) fn apply_process_group_management(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn apply_process_group_management(_cmd: &mut tokio::process::Command) {}

/// Send `SIGKILL` to the entire process group identified by `pgid`.
///
/// `ESRCH` (no such process group) is silently ignored — it means all
/// processes in the group have already exited. Other errors are logged at
/// debug level and do not propagate.
#[cfg(unix)]
pub(crate) fn kill_process_group(pgid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;

    if pgid == 0 {
        return;
    }
    if let Err(e) = killpg(Pid::from_raw(pgid as i32), Signal::SIGKILL) {
        if e != nix::errno::Errno::ESRCH {
            tracing::debug!("kill_process_group({}): {}", pgid, e);
        }
    }
}

/// RAII guard that sends `SIGKILL` to an entire process group on drop.
///
/// Create immediately after `spawn()` with the child's PID (which equals the
/// process group ID after `setsid()`). The guard fires even if the enclosing
/// `async fn` future is cancelled mid-execution, ensuring all grandchild
/// processes are cleaned up regardless of how the future is abandoned.
///
/// When the child process exits normally, killing its (now-empty) process group
/// returns `ESRCH` and is silently ignored. This means the guard is always-fire
/// and does not need to be explicitly disarmed.
pub(crate) struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: u32,
}

impl ProcessGroupGuard {
    pub(crate) fn new(pid: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            pgid: pid.unwrap_or(0),
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        kill_process_group(self.pgid);
    }
}
