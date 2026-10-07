//! Keep workspace ownership after a process cleanup deadline expires.
//!
//! A missing acknowledgement keeps its fence and retained owners alive. Dropping
//! the acknowledgement handle never authorizes reuse of an uncertain workspace.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;

type RetainedOwner = Arc<Mutex<Option<Box<dyn Send>>>>;

struct PendingCleanup {
    workspace: PathBuf,
    confirmed: AtomicBool,
    changed: Notify,
    owners: Mutex<Vec<RetainedOwner>>,
}

fn pending() -> &'static Mutex<Vec<Arc<PendingCleanup>>> {
    static PENDING: OnceLock<Mutex<Vec<Arc<PendingCleanup>>>> = OnceLock::new();
    PENDING.get_or_init(Mutex::default)
}

fn overlaps(workspace: Option<&Path>, pending: &Path) -> bool {
    // A vanished or inaccessible directory cannot prove that a pending writer
    // is unrelated. Keep ownership until the uncertain cleanup is resolved.
    workspace
        .is_none_or(|workspace| workspace.starts_with(pending) || pending.starts_with(workspace))
}

/// An explicit process-exit acknowledgement, created before relinquishing a
/// child to the background reaper. `workspace` must be canonicalized at spawn.
pub struct ProcessCleanupAcknowledgement(Arc<PendingCleanup>);

impl ProcessCleanupAcknowledgement {
    pub fn pending(workspace: PathBuf) -> Self {
        let state = Arc::new(PendingCleanup {
            workspace,
            confirmed: AtomicBool::new(false),
            changed: Notify::new(),
            owners: Mutex::new(Vec::new()),
        });
        pending().lock().unwrap().push(state.clone());
        Self(state)
    }

    /// Call only after the owned child and every process-group member are
    /// confirmed unable to run. Unconfirmed handle Drop deliberately does nothing.
    pub fn confirm(self) {
        {
            let mut pending = pending().lock().unwrap();
            pending.retain(|state| !Arc::ptr_eq(state, &self.0));
            self.0.confirmed.store(true, Ordering::Release);
        }
        self.0.changed.notify_waiters();
        // Releasing a workspace guard may inspect this registry. Drop retained
        // owners only after releasing both registry and owner-list locks.
        let owners = std::mem::take(&mut *self.0.owners.lock().unwrap());
        drop(owners);
    }
}

pub fn workspace_cleanup_pending(workspace: &Path) -> bool {
    let workspace = workspace.canonicalize().ok();
    pending()
        .lock()
        .unwrap()
        .iter()
        .any(|state| overlaps(workspace.as_deref(), &state.workspace))
}

/// Return the owner unchanged when no matching cleanup is pending. Otherwise
/// release it only after every currently matching process has acknowledged exit.
pub fn retain_until_workspace_cleanup<T: Send + 'static>(
    workspace: &Path,
    owner: T,
) -> Result<(), T> {
    let workspace = workspace.canonicalize().ok();
    let pending = pending().lock().unwrap();
    let matching: Vec<_> = pending
        .iter()
        .filter(|state| overlaps(workspace.as_deref(), &state.workspace))
        .collect();
    if matching.is_empty() {
        return Err(owner);
    }
    let retained: RetainedOwner = Arc::new(Mutex::new(Some(Box::new(owner))));
    for state in matching {
        state.owners.lock().unwrap().push(retained.clone());
    }
    Ok(())
}

pub async fn wait_for_workspace_cleanup(workspace: &Path) {
    loop {
        let workspace = workspace.canonicalize().ok();
        let states: Vec<_> = pending()
            .lock()
            .unwrap()
            .iter()
            .filter(|state| overlaps(workspace.as_deref(), &state.workspace))
            .cloned()
            .collect();
        if states.is_empty() {
            return;
        }
        for state in states {
            let changed = state.changed.notified();
            if !state.confirmed.load(Ordering::Acquire) {
                changed.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Owner(Arc<AtomicBool>);

    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn cleanup_confirmation_retains_owner_across_all_overlapping_children() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let first = ProcessCleanupAcknowledgement::pending(root.path().canonicalize().unwrap());
        let second = ProcessCleanupAcknowledgement::pending(child.canonicalize().unwrap());
        let released = Arc::new(AtomicBool::new(false));
        assert!(retain_until_workspace_cleanup(root.path(), Owner(released.clone())).is_ok());
        let wait = tokio::spawn({
            let root = root.path().to_path_buf();
            async move { wait_for_workspace_cleanup(&root).await }
        });
        first.confirm();
        assert!(!released.load(Ordering::Acquire));
        assert!(workspace_cleanup_pending(root.path()));
        assert!(!wait.is_finished());
        second.confirm();
        wait.await.unwrap();
        assert!(released.load(Ordering::Acquire));
        assert!(!workspace_cleanup_pending(root.path()));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_identity_includes_symlinks_and_ancestors_but_not_siblings() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(first.join("subdir")).unwrap();
        std::fs::create_dir(&second).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&first, &alias).unwrap();
        let cleanup = ProcessCleanupAcknowledgement::pending(first.canonicalize().unwrap());
        assert!(workspace_cleanup_pending(&alias));
        assert!(workspace_cleanup_pending(&alias.join("subdir")));
        assert!(workspace_cleanup_pending(root.path()));
        assert!(!workspace_cleanup_pending(&second));
        cleanup.confirm();
    }
}
