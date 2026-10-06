//! Linux process-group quiescence without waiting for another process to reap
//! dead descendants. A pidfd becoming readable confirms process exit even when
//! kill(-pgid, 0) still sees the zombie.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::raw::{c_int, c_long, c_short};

const SYS_PIDFD_OPEN: c_long = 434;
const ESRCH: i32 = 3;
const POLLIN: c_short = 1;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

extern "C" {
    fn getpgid(pid: c_int) -> c_int;
    fn syscall(number: c_long, ...) -> c_long;
    fn poll(fds: *mut PollFd, count: usize, timeout: c_int) -> c_int;
}

/// Returns true only after every visible member has an exit acknowledgement.
/// Missing process information, unsupported pidfds, and permission errors keep
/// ownership with the caller. No waitpid is used: the reaper owns those statuses.
pub(super) fn contains_only_exited_processes(group_id: u32) -> bool {
    if !procfs_matches_pid_namespace() {
        return false;
    }
    exited_group_members(group_id).is_some_and(|members| {
        !members.is_empty() && exited_group_members(group_id) == Some(members)
    })
}

fn exited_group_members(group_id: u32) -> Option<Vec<u32>> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // Query the kernel instead of relying on procfs stat fields, which can
        // be stale or virtualized independently of the current PID namespace.
        // SAFETY: getpgid only reads process metadata and takes an integer PID.
        let pgid = unsafe { getpgid(pid as c_int) };
        if pgid == -1 {
            if std::io::Error::last_os_error().raw_os_error() == Some(ESRCH) {
                continue;
            }
            return None;
        }
        if pgid != group_id as c_int {
            continue;
        }
        if !process_has_exited(pid)? {
            return None;
        }
        members.push(pid);
    }
    members.sort_unstable();
    Some(members)
}

// A host procfs mounted inside a different PID namespace cannot enumerate all
// descendants visible to getpgid/pidfd_open. In that case keep the group owned.
pub(super) fn procfs_matches_pid_namespace() -> bool {
    std::fs::read_link("/proc/self")
        .ok()
        .and_then(|path| path.file_name()?.to_str()?.parse::<u32>().ok())
        == Some(std::process::id())
}

fn process_has_exited(pid: u32) -> Option<bool> {
    // SAFETY: pidfd_open has two integer arguments on Linux x86_64/aarch64;
    // the returned descriptor is immediately wrapped in its unique owner.
    let raw_fd = unsafe { syscall(SYS_PIDFD_OPEN, pid as c_int, 0u32) };
    if raw_fd < 0 {
        return (std::io::Error::last_os_error().raw_os_error() == Some(ESRCH)).then_some(true);
    }
    // SAFETY: pidfd_open returned a newly owned descriptor above.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd as c_int) };
    let mut state = PollFd {
        fd: fd.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: state points to exactly one initialized pollfd for this call.
    match unsafe { poll(&mut state, 1, 0) } {
        0 => Some(false),
        1 if state.revents & POLLIN != 0 => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pidfd_reports_exit_before_the_owned_child_is_reaped() -> anyhow::Result<()> {
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("30").kill_on_drop(true);
        crate::set_process_group(&mut command);
        let mut child = command.spawn()?;
        let pid = child.id().expect("new child has a PID");
        assert_eq!(process_has_exited(pid), Some(false));
        child.start_kill()?;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while process_has_exited(pid) != Some(true) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert!(
            crate::process_group_has_members(pid),
            "the exited child is still unreaped"
        );
        child.wait().await?;
        assert!(!crate::process_group_has_members(pid));
        Ok(())
    }
}
