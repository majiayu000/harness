use crate::runtime_hosts::{ClaimCandidate, RuntimeHostManager};
use crate::task_runner::{TaskId, TaskStatus};

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

    let second = manager.claim_task("host-b", candidates, Some(30), None)?;
    assert!(second.is_some());
    Ok(())
}
