use super::*;

#[test]
fn public_output_limit_boundaries_are_exact() {
    for accepted in [1, RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES] {
        assert!(super::super::validate_output_limit(accepted).is_ok());
    }
    for rejected in [0, RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES + 1] {
        assert!(matches!(
            super::super::validate_output_limit(rejected),
            Err(RuntimeFingerprintProduceError::InvalidOutputLimit)
        ));
    }
}

#[tokio::test]
async fn terminal_first_path_candidate_never_derives_the_second_candidate() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    write_static_fixture(first.path(), "lazy-runtime");
    write_static_fixture(second.path(), "lazy-runtime");
    let configured = ConfiguredRuntimeExecutable::new(
        LocalExecutableRuntimeKind::CodexExec,
        source("lazy_resolution"),
        IsolationTier::Host,
        sandbox(SandboxMode::DangerFullAccess),
        "lazy-runtime",
        Vec::new(),
    );
    let child_path = std::env::join_paths([first.path(), second.path()]).unwrap();
    let options = RuntimeFingerprintOptions::new(first.path())
        .with_environment([(OsString::from("PATH"), child_path)]);
    let selected = environment::validate_and_select(
        configured.runtime_kind(),
        options.environment(),
        configured.setup_secret_env(),
    )
    .unwrap();
    let prepared = command::prepare_command(
        configured.executable().as_os_str(),
        options.working_dir(),
        selected.child_path.as_deref(),
    )
    .unwrap();
    let derivation_count = prepared.derivation_count();
    let envelope = super::super::owner::run(&configured, &options, selected, prepared)
        .await
        .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&envelope.to_json_string().unwrap()).unwrap();
    assert_eq!(
        json["payload"]["resolution_attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        derivation_count.load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn linux_pre_spawn_checkpoint_detects_in_place_content_change() {
    let registry = registry::OwnerRegistry::new();
    let directory = tempfile::tempdir().unwrap();
    let repository = tempfile::tempdir().unwrap();
    let executable = write_static_fixture(directory.path(), "changing-runtime");
    let deadline = std::time::Instant::now() + RUNTIME_FINGERPRINT_PROBE_DEADLINE;
    let working_directory =
        executable::observe_working_directory(directory.path(), deadline, &registry).unwrap();
    let command = command::prepare_command(executable.as_os_str(), directory.path(), None).unwrap();
    let retained = match candidate::observe_candidate(
        &command.candidate(0).unwrap().unwrap(),
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
            &command.candidate(0).unwrap().unwrap(),
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
    let prepared = command::prepare_command(
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
    let prepared = command::prepare_command(
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
