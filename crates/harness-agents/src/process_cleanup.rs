//! One process-wide reaper for cleanup that exceeded synchronous Drop's budget.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use harness_core::error::HarnessError;
use harness_core::process_cleanup::{workspace_cleanup_pending, ProcessCleanupAcknowledgement};

pub(super) struct DrainingChild {
    pub child: tokio::process::Child,
    pub child_reaped: bool,
    pub label: &'static str,
    pub process_group_id: Option<u32>,
    pub _egress_proxy_lease: Option<Arc<crate::spawn_contract::egress::EgressProxyLease>>,
    pub runtime: Option<tokio::runtime::Handle>,
    #[cfg(test)]
    pub confirmation_blocked: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl DrainingChild {
    pub fn try_complete(&mut self) -> bool {
        if !self.child_reaped {
            match self.child.try_wait() {
                Ok(Some(_)) => self.child_reaped = true,
                Ok(None) => return false,
                Err(error) => {
                    tracing::error!(
                        agent_process = self.label,
                        "process cleanup remains unconfirmed: {error}"
                    );
                    return false;
                }
            }
        }
        #[cfg(test)]
        if self
            .confirmation_blocked
            .as_ref()
            .is_some_and(|gate| gate.load(std::sync::atomic::Ordering::Acquire))
        {
            return false;
        }
        #[cfg(unix)]
        return self
            .process_group_id
            .is_none_or(|pid| !super::process_group_has_live_members(pid));
        #[cfg(not(unix))]
        true
    }
}

struct DeferredChild {
    child: DrainingChild,
    acknowledgement: Option<ProcessCleanupAcknowledgement>,
}

#[derive(Default)]
struct Reaper {
    children: Mutex<Vec<DeferredChild>>,
    changed: Condvar,
    started: OnceLock<Result<(), String>>,
}

fn reaper() -> &'static Reaper {
    static REAPER: OnceLock<Reaper> = OnceLock::new();
    REAPER.get_or_init(Reaper::default)
}

fn start_reaper() -> Result<(), String> {
    reaper()
        .started
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("harness-process-cleanup".into())
                .spawn(|| {
                    loop {
                        let mut children = reaper().children.lock().unwrap();
                        while children.is_empty() {
                            children = reaper().changed.wait(children).unwrap();
                        }
                        let batch = std::mem::take(&mut *children);
                        drop(children);
                        let mut waiting = Vec::new();
                        for mut child in batch {
                            if child.child.try_complete() {
                                tracing::info!(
                                    agent_process = child.child.label,
                                    "deferred process cleanup confirmed"
                                );
                                // Owner destructors schedule Docker and workspace cleanup.
                                // Enter the originating runtime so they do not execute
                                // blocking external commands on this sole reaper thread.
                                let runtime = child.child.runtime.clone();
                                let _entered = runtime.as_ref().map(|runtime| runtime.enter());
                                drop(child.child);
                                if let Some(acknowledgement) = child.acknowledgement {
                                    acknowledgement.confirm();
                                }
                            } else {
                                waiting.push(child);
                            }
                        }
                        let mut children = reaper().children.lock().unwrap();
                        children.extend(waiting);
                        let _ = reaper()
                            .changed
                            .wait_timeout(children, Duration::from_millis(25))
                            .unwrap();
                    }
                })
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .clone()
}

/// Validate at the actual spawn boundary, before starting a process. The
/// canonical host directory is also the identity retained by its cleanup owner.
pub(super) fn workspace_for_spawn(workspace: &Path) -> harness_core::error::Result<PathBuf> {
    start_reaper().map_err(|error| {
        HarnessError::AgentExecution(format!("process cleanup reaper is unavailable: {error}"))
    })?;
    let canonical = workspace.canonicalize().map_err(|error| {
        HarnessError::AgentExecution(format!(
            "cannot resolve agent workspace for process cleanup: {error}"
        ))
    })?;
    if workspace_cleanup_pending(&canonical) {
        return Err(HarnessError::AgentExecution(
            "workspace is reserved while agent process cleanup is unconfirmed".into(),
        ));
    }
    Ok(canonical)
}

pub(super) fn defer(child: DrainingChild, workspace: Option<PathBuf>) {
    // Publish the fence synchronously, before returning from Drop or releasing
    // any caller-owned workspace guard. Reaper startup failure keeps both the
    // child and all retained owners in this process-wide registry.
    let acknowledgement = workspace.map(ProcessCleanupAcknowledgement::pending);
    reaper().children.lock().unwrap().push(DeferredChild {
        child,
        acknowledgement,
    });
    if let Err(error) = start_reaper() {
        tracing::error!(
            "process cleanup is quarantined because the reaper could not start: {error}"
        );
    }
    reaper().changed.notify_one();
}
