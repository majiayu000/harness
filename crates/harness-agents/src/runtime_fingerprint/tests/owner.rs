use super::*;

fn register_child_for_test(
    registry: &registry::OwnerRegistry,
    pid: libc::pid_t,
    pidfd: libc::c_int,
    role: RuntimeOwnedChildRole,
) -> registry::RegisteredChild {
    let lease = registry.reserve_child_pidfd(role).unwrap();
    registry.register_child(lease, pid, pidfd, role).unwrap()
}

#[test]
fn ninth_owner_fails_with_the_global_capacity_reason() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _serialization = runtime.block_on(super::super::owner::TestOwnerPermit::acquire());
    let permits = (0..RUNTIME_FINGERPRINT_OWNER_CAPACITY)
        .map(|_| super::super::owner::OwnerPermit::try_acquire().unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        super::super::owner::OwnerPermit::try_acquire(),
        Err(RuntimeFingerprintProduceError::ContainmentUnavailable(
            ContainmentUnavailableReason::OwnerCapacityExhausted
        ))
    ));
    drop(permits);
    assert!(super::super::owner::OwnerPermit::try_acquire().is_ok());
}

#[test]
fn owner_descriptor_ledger_accepts_exact_capacity_and_releases_splits() {
    let registry = registry::OwnerRegistry::new();
    let mut lease = registry
        .reserve_descriptors(RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS)
        .unwrap();
    assert!(matches!(
        registry.reserve_descriptors(1),
        Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded)
    ));
    let split = lease
        .split_off(RUNTIME_FINGERPRINT_POST_READY_CHILD_REFERENCES)
        .unwrap();
    assert_eq!(
        registry.usage(),
        (0, RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS)
    );
    drop(split);
    assert_eq!(
        registry.usage(),
        (
            0,
            RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS
                - RUNTIME_FINGERPRINT_POST_READY_CHILD_REFERENCES
        )
    );
    drop(lease);
    assert_eq!(registry.usage(), (0, 0));
}

#[test]
fn owner_pidfd_ledger_reserves_one_target_and_one_helper_slot() {
    let registry = registry::OwnerRegistry::new();
    let target_role = RuntimeOwnedChildRole::InitialTarget;
    let helper_role = RuntimeOwnedChildRole::Observation(RuntimeObservationStage::Candidate);
    let target_lease = registry.reserve_child_pidfd(target_role).unwrap();
    let helper_lease = registry.reserve_child_pidfd(helper_role).unwrap();
    assert!(matches!(
        registry.reserve_child_pidfd(RuntimeOwnedChildRole::RetryTarget),
        Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded)
    ));
    assert!(matches!(
        registry.reserve_child_pidfd(RuntimeOwnedChildRole::Observation(
            RuntimeObservationStage::SourceHash
        )),
        Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded)
    ));
    let mut descriptors = [-1; 4];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    assert_eq!(
        unsafe { libc::pipe2(descriptors[2..].as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let target = registry
        .register_child(target_lease, 101, descriptors[0], target_role)
        .unwrap();
    let helper = registry
        .register_child(helper_lease, 102, descriptors[1], helper_role)
        .unwrap();
    probe::close_fd(descriptors[2]);
    probe::close_fd(descriptors[3]);
    target.reaped().unwrap();
    helper.reaped().unwrap();
    assert_eq!(registry.usage(), (0, 0));
}

#[test]
fn post_reservation_emfile_keeps_the_pidfd_open_stage_distinct() {
    let registry = registry::OwnerRegistry::new();
    let role = RuntimeOwnedChildRole::InitialTarget;
    let lease = registry.reserve_child_pidfd(role).unwrap();
    assert_eq!(registry.usage(), (1, 0));
    probe::inject_next_child_pidfd_open_errno(libc::EMFILE);
    assert_eq!(probe::open_child_pidfd(unsafe { libc::getpid() }), -1);
    assert_eq!(probe::last_errno(), libc::EMFILE);
    assert!(matches!(
        probe::registration_error(role, RuntimeChildRegistrationStage::PidfdOpen),
        RuntimeFingerprintProduceError::ChildRegistrationUnavailable {
            stage: RuntimeChildRegistrationStage::PidfdOpen,
            ..
        }
    ));
    drop(lease);
    assert_eq!(registry.usage(), (0, 0));
}

#[test]
fn child_pidfd_reservation_is_bound_to_its_declared_role() {
    let registry = registry::OwnerRegistry::new();
    let lease = registry
        .reserve_child_pidfd(RuntimeOwnedChildRole::InitialTarget)
        .unwrap();
    let mut descriptors = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    assert!(matches!(
        registry.register_child(
            lease,
            101,
            descriptors[0],
            RuntimeOwnedChildRole::RetryTarget,
        ),
        Err(RuntimeFingerprintProduceError::InvalidLaunchContext)
    ));
    probe::close_fd(descriptors[0]);
    probe::close_fd(descriptors[1]);
    assert_eq!(registry.usage(), (0, 0));
}

#[test]
fn global_post_ready_resource_ceiling_is_frozen() {
    assert_eq!(
        RUNTIME_FINGERPRINT_OWNER_CAPACITY * RUNTIME_FINGERPRINT_OWNER_PIDFD_SLOTS,
        16
    );
    assert_eq!(
        RUNTIME_FINGERPRINT_OWNER_CAPACITY
            * (RUNTIME_FINGERPRINT_OWNER_NON_PIDFD_SLOTS
                + RUNTIME_FINGERPRINT_POST_READY_CHILD_REFERENCES),
        320
    );
}

#[test]
fn ptrace_guard_options_require_exitkill_for_capability_and_target() {
    assert_ne!(probe::PTRACE_GUARD_OPTIONS & libc::PTRACE_O_EXITKILL, 0);
    assert_ne!(probe::PTRACE_GUARD_OPTIONS & libc::PTRACE_O_TRACEEXEC, 0);
    assert_ne!(probe::PTRACE_GUARD_OPTIONS & libc::PTRACE_O_TRACESYSGOOD, 0);
}

#[test]
fn surplus_rights_in_one_control_message_are_all_closed() {
    #[repr(align(16))]
    struct Control([u8; 64]);

    let mut descriptors = [-1; 4];
    assert_eq!(unsafe { libc::pipe2(descriptors.as_mut_ptr(), 0) }, 0);
    assert_eq!(unsafe { libc::pipe2(descriptors[2..].as_mut_ptr(), 0) }, 0);
    let mut control = Control([0; 64]);
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_control = control.0.as_mut_ptr().cast();
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE((2 * std::mem::size_of::<libc::c_int>()) as _) } as usize;
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    unsafe {
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN((2 * std::mem::size_of::<libc::c_int>()) as _) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast(), descriptors[0]);
        std::ptr::write_unaligned(
            libc::CMSG_DATA(header).cast::<libc::c_int>().add(1),
            descriptors[2],
        );
    }
    assert!(probe::take_exactly_one_received_right(&message).is_err());
    assert_eq!(unsafe { libc::fcntl(descriptors[0], libc::F_GETFD) }, -1);
    assert_eq!(probe::last_errno(), libc::EBADF);
    assert_eq!(unsafe { libc::fcntl(descriptors[2], libc::F_GETFD) }, -1);
    assert_eq!(probe::last_errno(), libc::EBADF);
    probe::close_fd(descriptors[1]);
    probe::close_fd(descriptors[3]);
}

#[test]
fn raw_kernel_signal_mask_blocks_nptl_reserved_signals_and_restores_exactly() {
    let saved = probe::block_all_signals().unwrap();
    let mut blocked = 0_u64;
    let observed = unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            std::ptr::null::<u64>(),
            &mut blocked,
            std::mem::size_of::<u64>(),
        )
    };
    let restored = probe::restore_signal_mask(saved);
    let mut after_restore = 0_u64;
    let restore_observed = unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            std::ptr::null::<u64>(),
            &mut after_restore,
            std::mem::size_of::<u64>(),
        )
    };
    assert_eq!(observed, 0);
    assert!(restored.is_ok());
    assert_eq!(restore_observed, 0);
    assert_ne!(blocked & (1_u64 << 31), 0);
    assert_ne!(blocked & (1_u64 << 32), 0);
    assert_eq!(after_restore, saved);
}

#[test]
fn failed_registry_commit_reaps_gated_child_before_any_workload() {
    let registry = registry::OwnerRegistry::new();
    let foreign_registry = registry::OwnerRegistry::new();
    let role = RuntimeOwnedChildRole::InitialTarget;
    let mismatched_lease = foreign_registry.reserve_child_pidfd(role).unwrap();

    let mut gate = [-1; 2];
    let mut workload = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(gate.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    assert_eq!(
        unsafe { libc::pipe2(workload.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        probe::close_fd(gate[1]);
        probe::close_fd(workload[0]);
        let mut go = 0_u8;
        let read = unsafe { libc::read(gate[0], (&mut go as *mut u8).cast(), 1) };
        if read == 1 && go == probe::CHILD_GO {
            let marker = 1_u8;
            unsafe {
                libc::write(workload[1], (&marker as *const u8).cast(), 1);
            }
        }
        unsafe { libc::_exit(0) };
    }
    probe::close_fd(gate[0]);
    probe::close_fd(workload[1]);
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    assert!(pidfd >= 0);
    let result = probe::register_child(&registry, mismatched_lease, pid, pidfd, role);
    assert!(matches!(
        result,
        Err(
            RuntimeFingerprintProduceError::ChildRegistrationUnavailable {
                stage: RuntimeChildRegistrationStage::RegistryCommit,
                ..
            }
        )
    ));
    probe::close_fd(gate[1]);
    let mut marker = 0_u8;
    assert_eq!(
        unsafe { libc::read(workload[0], (&mut marker as *mut u8).cast(), 1) },
        0
    );
    probe::close_fd(workload[0]);
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(probe::last_errno(), libc::ECHILD);
    assert_eq!(registry.usage(), (0, 0));
    assert_eq!(foreign_registry.usage(), (0, 0));
}

#[test]
fn expired_cleanup_retains_obligation_until_owner_drain_reaps() {
    let registry = registry::OwnerRegistry::new();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    assert!(pidfd >= 0);
    let child =
        register_child_for_test(&registry, pid, pidfd, RuntimeOwnedChildRole::InitialTarget);
    assert!(matches!(
        child.cleanup(std::time::Instant::now()),
        Err(
            RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                operation: RuntimeChildCleanupOperation::Reap,
                ..
            }
        )
    ));
    assert_eq!(registry.usage(), (1, 0));
    registry.drain_retained();
    assert_eq!(registry.usage(), (0, 0));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(probe::last_errno(), libc::ECHILD);
}

#[test]
fn registered_failure_cleanup_uses_a_fresh_cleanup_window() {
    let registry = registry::OwnerRegistry::new();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int };
    assert!(pidfd >= 0);
    let child =
        register_child_for_test(&registry, pid, pidfd, RuntimeOwnedChildRole::InitialTarget);
    probe::cleanup_registered_child(child).unwrap();
    assert!(registry.is_empty());
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(probe::last_errno(), libc::ECHILD);
}

#[test]
fn expired_unregistered_cleanup_retains_pid_until_owner_drain_reaps() {
    let registry = registry::OwnerRegistry::new();
    let role = RuntimeOwnedChildRole::InitialTarget;
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    assert!(matches!(
        probe::rollback_unregistered_child_before(&registry, pid, std::time::Instant::now(), role,),
        Err(
            RuntimeFingerprintProduceError::ChildRegistrationCleanupIncomplete {
                operation: RuntimeChildCleanupOperation::Reap,
                ..
            }
        )
    ));
    assert_eq!(registry.pre_registration_usage(), 1);
    assert!(!registry.is_empty());
    registry.drain_retained();
    assert_eq!(registry.pre_registration_usage(), 0);
    assert!(registry.is_empty());
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(probe::last_errno(), libc::ECHILD);
}
