use crate::runtime_hosts::{ClaimCandidate, RuntimeHostManager};
use crate::task_runner::{TaskId, TaskStatus};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Barrier,
    },
    thread,
    time::Duration,
};

fn candidate(task_id: TaskId, created_at: &str) -> ClaimCandidate {
    ClaimCandidate {
        task_id,
        status: TaskStatus::Pending,
        created_at: Some(created_at.to_string()),
        project: None,
    }
}

#[test]
fn active_lease_blocks_double_claim() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_timeouts(60, 30);
    manager.register("host-a".to_string(), None, vec![]);
    manager.register("host-b".to_string(), None, vec![]);

    let task_id = TaskId::new();
    let candidates = vec![candidate(task_id.clone(), "2026-04-02T00:00:00Z")];

    let first = manager.claim_task("host-a", candidates.clone(), Some(30), None)?;
    assert!(first.is_some());
    let second = manager.claim_task("host-b", candidates, Some(30), None)?;
    assert!(second.is_none());
    Ok(())
}

#[test]
fn zero_ttl_lease_is_reclaimable() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_timeouts(60, 30);
    manager.register("host-a".to_string(), None, vec![]);
    manager.register("host-b".to_string(), None, vec![]);

    let task_id = TaskId::new();
    let candidates = vec![candidate(task_id.clone(), "2026-04-02T00:00:00Z")];

    let first = manager.claim_task("host-a", candidates.clone(), Some(0), None)?;
    assert!(first.is_some());
    let second = manager.claim_task("host-b", candidates, Some(30), None)?;
    assert!(second.is_some());
    Ok(())
}

#[test]
fn deregister_releases_owned_leases() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_timeouts(60, 30);
    manager.register("host-a".to_string(), None, vec![]);
    manager.register("host-b".to_string(), None, vec![]);

    let task_id = TaskId::new();
    let candidates = vec![candidate(task_id.clone(), "2026-04-02T00:00:00Z")];

    let first = manager.claim_task("host-a", candidates.clone(), Some(30), None)?;
    assert!(first.is_some());
    assert!(manager.deregister("host-a"));
    assert!(manager.host_leases.get("host-a").is_none());

    let second = manager.claim_task("host-b", candidates, Some(30), None)?;
    assert!(second.is_some());
    Ok(())
}

#[test]
fn deregister_compacts_lease_expiry_heap() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_timeouts(60, 300);
    manager.register("host-a".to_string(), None, vec![]);

    let task_id = TaskId::new();
    let first = manager.claim_task_id("host-a", &task_id, Some(300))?;
    assert!(first.is_some());
    assert_eq!(
        manager
            .lease_expirations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len(),
        1
    );

    assert!(manager.deregister("host-a"));
    assert_eq!(
        manager
            .lease_expirations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len(),
        0
    );
    Ok(())
}

#[test]
fn deregister_waits_for_lease_lock_before_removing_host() {
    let manager = Arc::new(RuntimeHostManager::with_timeouts(60, 30));
    manager.register("host-a".to_string(), None, vec![]);

    let lease_guard = manager.hold_lease_mutation_lock_for_test();
    let barrier = Arc::new(Barrier::new(2));
    let finished = Arc::new(AtomicBool::new(false));

    let manager_for_thread = Arc::clone(&manager);
    let barrier_for_thread = Arc::clone(&barrier);
    let finished_for_thread = Arc::clone(&finished);
    let join = thread::spawn(move || {
        barrier_for_thread.wait();
        manager_for_thread.deregister("host-a");
        finished_for_thread.store(true, Ordering::SeqCst);
    });

    barrier.wait();

    for _ in 0..25 {
        assert!(manager.hosts.contains_key("host-a"));
        assert!(!finished.load(Ordering::SeqCst));
        thread::sleep(Duration::from_millis(2));
    }

    drop(lease_guard);
    join.join().unwrap();

    assert!(!manager.hosts.contains_key("host-a"));
}

#[test]
fn active_lease_count_before_claim() {
    let manager = RuntimeHostManager::with_timeouts(60, 30);
    manager.register("host-a".to_string(), None, vec![]);
    assert_eq!(manager.active_lease_count("host-a"), 0);
}

#[test]
fn active_lease_count_after_claim() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_timeouts(60, 30);
    manager.register("host-a".to_string(), None, vec![]);

    let task_id = TaskId::new();
    let result = manager.claim_task_id("host-a", &task_id, Some(60))?;
    assert!(result.is_some());
    assert_eq!(manager.active_lease_count("host-a"), 1);
    Ok(())
}

#[test]
fn active_lease_count_after_deregister() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_timeouts(60, 30);
    manager.register("host-a".to_string(), None, vec![]);

    let task_id = TaskId::new();
    manager.claim_task_id("host-a", &task_id, Some(60))?;
    assert_eq!(manager.active_lease_count("host-a"), 1);

    manager.deregister("host-a");
    assert_eq!(manager.active_lease_count("host-a"), 0);
    Ok(())
}
