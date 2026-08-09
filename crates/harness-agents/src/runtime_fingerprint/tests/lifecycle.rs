use super::*;

#[test]
fn linux_pre_spawn_checkpoint_detects_in_place_content_change() {
    let registry = registry::OwnerRegistry::new();
    let directory = tempfile::tempdir().unwrap();
    let repository = tempfile::tempdir().unwrap();
    let executable = write_static_fixture(directory.path(), "changing-runtime");
    let deadline = std::time::Instant::now() + RUNTIME_FINGERPRINT_PROBE_DEADLINE;
    let working_directory =
        executable::observe_working_directory(directory.path(), deadline, &registry).unwrap();
    let command =
        executable::prepare_command(executable.as_os_str(), directory.path(), None).unwrap();
    let retained = match candidate::observe_candidate(
        &command.candidates[0],
        &working_directory,
        deadline,
        &registry,
    )
    .unwrap()
    {
        candidate::CandidateObservation::Retained(retained) => retained,
        other => panic!("expected retained candidate, got {other:?}"),
    };
    let mut changed = static_elf_fixture(if cfg!(target_arch = "x86_64") {
        62
    } else {
        183
    });
    changed.push(1);
    std::fs::write(&executable, changed).unwrap();
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    assert_eq!(
        checkpoint::pre_spawn(
            &command.candidates[0],
            &working_directory,
            &retained,
            &boundaries,
            deadline,
            &registry,
        )
        .unwrap(),
        checkpoint::PreSpawnCheckpoint::IdentityChanged
    );
    drop((retained, working_directory));
    assert_eq!(registry.usage(), (0, 0));
}

#[test]
fn dropping_hosting_tokio_runtime_still_reaps_registered_target() {
    let repository = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let executable = test_fixtures::write_loop_fixture(external.path(), "loop-runtime");
    let configured = configured_path(&executable);
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    let options = RuntimeFingerprintOptions::new(std::env::current_dir().unwrap())
        .with_repository_boundaries(boundaries);
    let selected = environment::validate_and_select(
        configured.runtime_kind(),
        &options.environment,
        configured.setup_secret_env(),
    )
    .unwrap();
    let prepared = executable::prepare_command(
        configured.executable().as_os_str(),
        options.working_dir(),
        selected.child_path.as_deref(),
    )
    .unwrap();
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .unwrap();
    runtime.spawn(async move {
        super::super::owner::run_observed(&configured, &options, selected, prepared, events_tx)
            .await
    });

    let event_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let remaining = event_deadline.saturating_duration_since(std::time::Instant::now());
        let event = events_rx.recv_timeout(remaining).unwrap();
        if matches!(
            event,
            super::super::owner::OwnerLifecycleEvent::ChildRegistered {
                role: RuntimeOwnedChildRole::InitialTarget,
                ..
            }
        ) {
            break;
        }
    }
    drop(runtime);

    let mut target_reaped = false;
    loop {
        let remaining = event_deadline.saturating_duration_since(std::time::Instant::now());
        match events_rx.recv_timeout(remaining).unwrap() {
            super::super::owner::OwnerLifecycleEvent::ChildReaped {
                role: RuntimeOwnedChildRole::InitialTarget,
                ..
            } => {
                target_reaped = true;
            }
            super::super::owner::OwnerLifecycleEvent::OwnerExited => break,
            _ => {}
        }
    }
    assert!(target_reaped);
}

struct SetpgidChurner(libc::pid_t);

impl SetpgidChurner {
    fn spawn() -> Self {
        let mut ready = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(ready.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            probe::close_fd(ready[0]);
            unsafe {
                libc::setpgid(0, 0);
                let marker = 1_u8;
                libc::write(ready[1], (&marker as *const u8).cast(), 1);
                libc::close(ready[1]);
                loop {
                    libc::setpgid(0, 0);
                    libc::sched_yield();
                }
            }
        }
        probe::close_fd(ready[1]);
        let mut marker = 0_u8;
        assert_eq!(
            unsafe { libc::read(ready[0], (&mut marker as *mut u8).cast(), 1) },
            1
        );
        probe::close_fd(ready[0]);
        assert_eq!(marker, 1);
        Self(pid)
    }

    const fn pid(&self) -> libc::pid_t {
        self.0
    }

    fn is_alive(&self) -> bool {
        unsafe { libc::kill(self.0, 0) == 0 }
    }
}

impl Drop for SetpgidChurner {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            loop {
                if libc::waitpid(self.0, std::ptr::null_mut(), 0) == self.0
                    || probe::last_errno() != libc::EINTR
                {
                    break;
                }
            }
        }
    }
}

#[test]
fn unrelated_same_session_setpgid_churn_never_enters_owner_registry() {
    let churner = SetpgidChurner::spawn();
    assert_eq!(unsafe { libc::getsid(churner.pid()) }, unsafe {
        libc::getsid(0)
    });

    let repository = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let executable = write_static_fixture(external.path(), "version-runtime");
    let configured = configured_path(&executable);
    let boundaries = ValidatedRepositoryBoundarySet::from_existing_roots(
        repository.path(),
        std::iter::empty::<&Path>(),
    )
    .unwrap();
    let options = RuntimeFingerprintOptions::new(std::env::current_dir().unwrap())
        .with_repository_boundaries(boundaries);
    let selected = environment::validate_and_select(
        configured.runtime_kind(),
        &options.environment,
        configured.setup_secret_env(),
    )
    .unwrap();
    let prepared = executable::prepare_command(
        configured.executable().as_os_str(),
        options.working_dir(),
        selected.child_path.as_deref(),
    )
    .unwrap();
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .unwrap();
    runtime
        .block_on(super::super::owner::run_observed(
            &configured,
            &options,
            selected,
            prepared,
            events_tx,
        ))
        .unwrap();

    let mut saw_owner_exit = false;
    while let Ok(event) = events_rx.recv_timeout(std::time::Duration::from_secs(3)) {
        match event {
            super::super::owner::OwnerLifecycleEvent::ChildRegistered { pid, .. }
            | super::super::owner::OwnerLifecycleEvent::ChildReaped { pid, .. } => {
                assert_ne!(pid, churner.pid());
            }
            super::super::owner::OwnerLifecycleEvent::OwnerExited => {
                saw_owner_exit = true;
                break;
            }
        }
    }
    assert!(saw_owner_exit);
    assert!(churner.is_alive());
}
