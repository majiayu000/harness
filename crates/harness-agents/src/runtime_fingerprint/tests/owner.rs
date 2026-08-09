use super::*;

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
fn owner_pidfd_ledger_accepts_two_and_rejects_third() {
    let registry = registry::OwnerRegistry::new();
    let mut descriptors = [-1; 4];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    assert_eq!(
        unsafe { libc::pipe2(descriptors[2..].as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let role = RuntimeOwnedChildRole::InitialTarget;
    let first = registry.register_child(101, descriptors[0], role).unwrap();
    let second = registry.register_child(102, descriptors[1], role).unwrap();
    assert!(matches!(
        registry.register_child(103, descriptors[2], role),
        Err(RuntimeFingerprintProduceError::OwnerResourceCapacityExceeded)
    ));
    probe::close_fd(descriptors[2]);
    probe::close_fd(descriptors[3]);
    first.reaped().unwrap();
    second.reaped().unwrap();
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
fn failed_registry_commit_reaps_gated_child_before_any_workload() {
    let registry = registry::OwnerRegistry::new();
    let mut occupied = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(occupied.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let first = registry
        .register_child(101, occupied[0], RuntimeOwnedChildRole::InitialTarget)
        .unwrap();
    let second = registry
        .register_child(102, occupied[1], RuntimeOwnedChildRole::RetryTarget)
        .unwrap();

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
    let result = probe::register_child(
        &registry,
        pid,
        pidfd,
        std::time::Instant::now() + RUNTIME_FINGERPRINT_CLEANUP_DEADLINE,
        RuntimeOwnedChildRole::InitialTarget,
    );
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
    first.reaped().unwrap();
    second.reaped().unwrap();
    assert_eq!(registry.usage(), (0, 0));
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
    let child = registry
        .register_child(pid, pidfd, RuntimeOwnedChildRole::InitialTarget)
        .unwrap();
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
