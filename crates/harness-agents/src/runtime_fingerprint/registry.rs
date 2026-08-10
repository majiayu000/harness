//! Owner-local child and descriptor resource ledger.

use super::{
    RuntimeChildCleanupOperation, RuntimeFingerprintProduceError, RuntimeOwnedChildRole,
    RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS, RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct ChildEntry {
    pid: libc::pid_t,
    pidfd: libc::c_int,
    role: RuntimeOwnedChildRole,
}

#[derive(Debug, Clone, Copy)]
struct PreRegistrationEntry {
    pid: libc::pid_t,
    role: RuntimeOwnedChildRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidfdSlot {
    Subject,
    Helper,
}

impl PidfdSlot {
    const fn for_role(role: Option<RuntimeOwnedChildRole>) -> Self {
        match role {
            Some(
                RuntimeOwnedChildRole::InitialTarget
                | RuntimeOwnedChildRole::RetryTarget
                | RuntimeOwnedChildRole::Observation(super::RuntimeObservationStage::CapabilityCheck),
            ) => Self::Subject,
            Some(RuntimeOwnedChildRole::Observation(_)) | None => Self::Helper,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Subject => 0,
            Self::Helper => 1,
        }
    }
}

#[derive(Debug)]
struct RegistryInner {
    children: [Option<ChildEntry>; RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS],
    pre_registration: [Option<PreRegistrationEntry>; RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS + 1],
    reserved_pidfds: [bool; RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS],
    non_pidfds: usize,
}

#[derive(Debug, Clone)]
pub(super) struct OwnerRegistry {
    inner: Rc<RefCell<RegistryInner>>,
    observer: Option<std::sync::mpsc::Sender<super::owner::OwnerLifecycleEvent>>,
}

impl OwnerRegistry {
    pub(super) fn new() -> Self {
        Self::with_observer(None)
    }

    pub(super) fn with_observer(
        observer: Option<std::sync::mpsc::Sender<super::owner::OwnerLifecycleEvent>>,
    ) -> Self {
        Self {
            inner: Rc::new(RefCell::new(RegistryInner {
                children: std::array::from_fn(|_| None),
                pre_registration: std::array::from_fn(|_| None),
                reserved_pidfds: [false; RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS],
                non_pidfds: 0,
            })),
            observer,
        }
    }

    pub(super) fn reserve_descriptors(
        &self,
        count: usize,
    ) -> Result<DescriptorLease, RuntimeFingerprintProduceError> {
        let mut inner = self.inner.borrow_mut();
        let next = inner
            .non_pidfds
            .checked_add(count)
            .ok_or(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded)?;
        if next > RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS {
            return Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded);
        }
        inner.non_pidfds = next;
        Ok(DescriptorLease {
            registry: self.clone(),
            count,
        })
    }

    pub(super) fn register_child(
        &self,
        mut pidfd_lease: PidfdLease,
        pid: libc::pid_t,
        pidfd: libc::c_int,
        role: RuntimeOwnedChildRole,
    ) -> Result<RegisteredChild, RuntimeFingerprintProduceError> {
        if pid <= 0
            || pidfd < 0
            || !pidfd_lease.active
            || pidfd_lease.role != Some(role)
            || !Rc::ptr_eq(&self.inner, &pidfd_lease.registry.inner)
        {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        let slot = {
            let mut inner = self.inner.borrow_mut();
            let Some(slot) = inner.children.iter().position(Option::is_none) else {
                return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
            };
            let reserved_slot = PidfdSlot::for_role(pidfd_lease.role).index();
            if !inner.reserved_pidfds[reserved_slot] {
                return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
            }
            inner.reserved_pidfds[reserved_slot] = false;
            inner.children[slot] = Some(ChildEntry { pid, pidfd, role });
            slot
        };
        pidfd_lease.active = false;
        self.notify(super::owner::OwnerLifecycleEvent::ChildRegistered { pid, role });
        Ok(RegisteredChild {
            registry: self.clone(),
            slot,
            pid,
            pidfd,
            role,
        })
    }

    pub(super) fn reserve_pidfd(&self) -> Result<PidfdLease, RuntimeFingerprintProduceError> {
        self.reserve_pidfd_for_role(None)
    }

    pub(super) fn reserve_child_pidfd(
        &self,
        role: RuntimeOwnedChildRole,
    ) -> Result<PidfdLease, RuntimeFingerprintProduceError> {
        self.reserve_pidfd_for_role(Some(role))
    }

    fn reserve_pidfd_for_role(
        &self,
        role: Option<RuntimeOwnedChildRole>,
    ) -> Result<PidfdLease, RuntimeFingerprintProduceError> {
        let mut inner = self.inner.borrow_mut();
        let slot = PidfdSlot::for_role(role);
        let occupied = inner
            .children
            .iter()
            .flatten()
            .any(|entry| PidfdSlot::for_role(Some(entry.role)) == slot);
        if occupied || inner.reserved_pidfds[slot.index()] {
            return Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded);
        }
        inner.reserved_pidfds[slot.index()] = true;
        Ok(PidfdLease {
            registry: self.clone(),
            role,
            active: true,
        })
    }

    pub(super) fn retain_pre_registration_child(
        &self,
        pid: libc::pid_t,
        role: RuntimeOwnedChildRole,
    ) -> Result<(), RuntimeFingerprintProduceError> {
        if pid <= 0 {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        let mut inner = self.inner.borrow_mut();
        if inner
            .children
            .iter()
            .flatten()
            .any(|entry| entry.pid == pid)
        {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        if let Some(entry) = inner
            .pre_registration
            .iter()
            .flatten()
            .find(|entry| entry.pid == pid)
        {
            return (entry.role == role)
                .then_some(())
                .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        let Some(slot) = inner.pre_registration.iter().position(Option::is_none) else {
            return Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded);
        };
        inner.pre_registration[slot] = Some(PreRegistrationEntry { pid, role });
        Ok(())
    }

    pub(super) fn cleanup_pre_registration_child(
        &self,
        pid: libc::pid_t,
        role: RuntimeOwnedChildRole,
        deadline: Instant,
    ) -> Result<(), RuntimeFingerprintProduceError> {
        let retained = self
            .inner
            .borrow()
            .pre_registration
            .iter()
            .flatten()
            .any(|entry| entry.pid == pid && entry.role == role);
        if !retained {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        if Instant::now() >= deadline {
            return Err(pre_registration_cleanup_error(
                role,
                RuntimeChildCleanupOperation::Reap,
            ));
        }
        // SAFETY: the registry proves this exact positive PID is an unreaped direct child.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 && last_errno() != libc::ESRCH {
            return Err(pre_registration_cleanup_error(
                role,
                RuntimeChildCleanupOperation::Termination,
            ));
        }
        loop {
            let mut status = 0;
            // SAFETY: the same registry obligation remains live until this wait succeeds.
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == pid {
                self.remove_pre_registration_child(pid, role);
                return Ok(());
            }
            if result < 0 {
                let errno = last_errno();
                if errno == libc::EINTR && Instant::now() < deadline {
                    continue;
                }
                if errno == libc::ECHILD {
                    self.remove_pre_registration_child(pid, role);
                }
                return Err(pre_registration_cleanup_error(
                    role,
                    RuntimeChildCleanupOperation::Reap,
                ));
            }
            if Instant::now() >= deadline {
                return Err(pre_registration_cleanup_error(
                    role,
                    RuntimeChildCleanupOperation::Reap,
                ));
            }
            super::probe::pause_for_status_check(deadline).map_err(|()| {
                pre_registration_cleanup_error(role, RuntimeChildCleanupOperation::Reap)
            })?;
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        let inner = self.inner.borrow();
        inner.children.iter().all(Option::is_none)
            && inner.pre_registration.iter().all(Option::is_none)
            && inner.reserved_pidfds.iter().all(|reserved| !reserved)
            && inner.non_pidfds == 0
    }

    #[cfg(test)]
    pub(super) fn usage(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        (
            inner
                .children
                .iter()
                .filter(|entry| entry.is_some())
                .count()
                + inner
                    .reserved_pidfds
                    .iter()
                    .filter(|reserved| **reserved)
                    .count(),
            inner.non_pidfds,
        )
    }

    #[cfg(test)]
    pub(super) fn pre_registration_usage(&self) -> usize {
        self.inner
            .borrow()
            .pre_registration
            .iter()
            .flatten()
            .count()
    }

    pub(super) fn drain_retained(&self) {
        while !self.is_empty() {
            self.cleanup_before(Instant::now() + Duration::from_millis(250));
            if !self.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn cleanup_before(&self, deadline: Instant) {
        let pre_registration = {
            let inner = self.inner.borrow();
            inner
                .pre_registration
                .iter()
                .flatten()
                .map(|entry| (entry.pid, entry.role))
                .collect::<Vec<_>>()
        };
        for (pid, role) in pre_registration {
            if let Err(error) = self.cleanup_pre_registration_child(pid, role, deadline) {
                tracing::debug!(
                    ?error,
                    "pre-registration child cleanup remains pending for owner drain"
                );
            }
        }
        let entries = {
            let inner = self.inner.borrow();
            inner
                .children
                .iter()
                .enumerate()
                .filter_map(|(slot, entry)| {
                    entry
                        .as_ref()
                        .map(|entry| (slot, entry.pid, entry.pidfd, entry.role))
                })
                .collect::<Vec<_>>()
        };
        for (slot, pid, pidfd, _) in entries {
            let signal = pidfd_send_kill(pidfd);
            if signal.is_err() && last_errno() != libc::ESRCH {
                continue;
            }
            if matches!(waitid_pidfd(pidfd, deadline), Ok(seen) if seen == pid) {
                self.remove_and_close(slot, pid, pidfd);
            }
        }
    }

    fn remove_pre_registration_child(&self, pid: libc::pid_t, role: RuntimeOwnedChildRole) -> bool {
        let mut inner = self.inner.borrow_mut();
        let Some(slot) = inner
            .pre_registration
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.pid == pid && entry.role == role))
        else {
            return false;
        };
        inner.pre_registration[slot] = None;
        true
    }

    fn remove_and_close(&self, slot: usize, pid: libc::pid_t, pidfd: libc::c_int) -> bool {
        let removed_role = {
            let mut inner = self.inner.borrow_mut();
            match inner.children.get(slot).and_then(Option::as_ref) {
                Some(entry) if entry.pid == pid && entry.pidfd == pidfd => {
                    let role = entry.role;
                    inner.children[slot] = None;
                    Some(role)
                }
                _ => None,
            }
        };
        if let Some(role) = removed_role {
            close_fd(pidfd);
            self.notify(super::owner::OwnerLifecycleEvent::ChildReaped { pid, role });
        }
        removed_role.is_some()
    }

    fn release_descriptors(&self, count: usize) {
        let mut inner = self.inner.borrow_mut();
        debug_assert!(inner.non_pidfds >= count);
        inner.non_pidfds = inner.non_pidfds.saturating_sub(count);
    }

    fn release_pidfd(&self, role: Option<RuntimeOwnedChildRole>) {
        let mut inner = self.inner.borrow_mut();
        let slot = PidfdSlot::for_role(role).index();
        debug_assert!(inner.reserved_pidfds[slot]);
        inner.reserved_pidfds[slot] = false;
    }

    fn notify(&self, event: super::owner::OwnerLifecycleEvent) {
        if let Some(observer) = &self.observer {
            if observer.send(event).is_err() {
                tracing::debug!("runtime fingerprint owner observer dropped");
            }
        }
    }
}

fn pre_registration_cleanup_error(
    role: RuntimeOwnedChildRole,
    operation: RuntimeChildCleanupOperation,
) -> RuntimeFingerprintProduceError {
    RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete { role, operation }
}

#[derive(Debug)]
pub(super) struct PidfdLease {
    registry: OwnerRegistry,
    role: Option<RuntimeOwnedChildRole>,
    active: bool,
}

impl Drop for PidfdLease {
    fn drop(&mut self) {
        if self.active {
            self.registry.release_pidfd(self.role);
        }
    }
}

#[derive(Debug)]
pub(super) struct DescriptorLease {
    registry: OwnerRegistry,
    count: usize,
}

impl DescriptorLease {
    pub(super) fn split_off(
        &mut self,
        count: usize,
    ) -> Result<Self, RuntimeFingerprintProduceError> {
        if count > self.count {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
        self.count -= count;
        Ok(Self {
            registry: self.registry.clone(),
            count,
        })
    }
}

impl Drop for DescriptorLease {
    fn drop(&mut self) {
        self.registry.release_descriptors(self.count);
    }
}

#[derive(Debug)]
pub(super) struct RegisteredChild {
    registry: OwnerRegistry,
    slot: usize,
    pid: libc::pid_t,
    pidfd: libc::c_int,
    role: RuntimeOwnedChildRole,
}

impl RegisteredChild {
    pub(super) const fn pid(&self) -> libc::pid_t {
        self.pid
    }

    pub(super) const fn pidfd(&self) -> libc::c_int {
        self.pidfd
    }

    pub(super) fn reaped(self) -> Result<(), RuntimeFingerprintProduceError> {
        if !self
            .registry
            .remove_and_close(self.slot, self.pid, self.pidfd)
        {
            return Err(RuntimeFingerprintProduceError::ExecutionVerificationUnavailable);
        }
        Ok(())
    }

    pub(super) fn cleanup(self, deadline: Instant) -> Result<(), RuntimeFingerprintProduceError> {
        if Instant::now() >= deadline {
            return Err(
                RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                    role: self.role,
                    operation: RuntimeChildCleanupOperation::Reap,
                },
            );
        }
        let signal = pidfd_send_kill(self.pidfd);
        if signal.is_err() && last_errno() != libc::ESRCH {
            return Err(
                RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                    role: self.role,
                    operation: RuntimeChildCleanupOperation::Termination,
                },
            );
        }
        match waitid_pidfd(self.pidfd, deadline) {
            Ok(seen) if seen == self.pid => self.reaped(),
            _ => Err(
                RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                    role: self.role,
                    operation: RuntimeChildCleanupOperation::Reap,
                },
            ),
        }
    }
}

fn pidfd_send_kill(pidfd: libc::c_int) -> Result<(), ()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    (result == 0).then_some(()).ok_or(())
}

fn waitid_pidfd(pidfd: libc::c_int, deadline: Instant) -> Result<libc::pid_t, ()> {
    let mut descriptor = [libc::pollfd {
        fd: pidfd,
        events: libc::POLLIN,
        revents: 0,
    }];
    super::probe::poll_until_ready(&mut descriptor, deadline).map_err(|_| ())?;
    loop {
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pidfd as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG,
            )
        };
        if result != 0 {
            if last_errno() == libc::EINTR && Instant::now() < deadline {
                continue;
            }
            return Err(());
        }
        let seen = unsafe { info.si_pid() };
        if seen != 0 {
            return Ok(seen);
        }
        return Err(());
    }
}

fn close_fd(fd: libc::c_int) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

fn last_errno() -> libc::c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
