use super::*;
use std::time::Duration;
use tokio::time::timeout;

fn config(max_concurrent: usize, max_queue: usize) -> ConcurrencyConfig {
    ConcurrencyConfig {
        max_concurrent_tasks: max_concurrent,
        max_queue_size: max_queue,
        stall_timeout_secs: 300,
        per_project: Default::default(),
        ..ConcurrencyConfig::default()
    }
}

#[tokio::test]
async fn acquire_and_release_single_permit() {
    let q = TaskQueue::new(&config(2, 8));
    assert_eq!(q.running_count(), 0);
    let permit = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 1);
    drop(permit);
    assert_eq!(q.running_count(), 0);
}

#[tokio::test]
async fn only_max_concurrent_tasks_run_simultaneously() {
    let q = Arc::new(TaskQueue::new(&config(2, 16)));

    // Acquire both permits.
    let p1 = q.acquire("proj", 0).await.unwrap();
    let p2 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);

    // Third acquire must block; verify with a short timeout.
    let q2 = q.clone();
    let blocked = timeout(Duration::from_millis(50), q2.acquire("proj", 0)).await;
    assert!(blocked.is_err(), "third acquire should block when limit=2");

    // Release a slot; third acquire should now succeed.
    drop(p1);
    let p3 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
    drop(p2);
    drop(p3);
    assert_eq!(q.running_count(), 0);
}

#[tokio::test]
async fn queue_overflow_returns_error() {
    // 1 concurrent slot, queue capacity 2.
    let q = Arc::new(TaskQueue::new(&config(1, 2)));

    // Hold the single execution slot.
    let _p1 = q.acquire("proj", 0).await.unwrap();

    // Two tasks queue up (they block, so spawn them).
    let q2 = q.clone();
    let _h1 = tokio::spawn(async move { q2.acquire("proj", 0).await });
    let q3 = q.clone();
    let _h2 = tokio::spawn(async move { q3.acquire("proj", 0).await });

    // Give spawned tasks time to increment queued_count.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Third waiter exceeds max_queue_size=2 → error.
    let result = q.acquire("proj", 0).await;
    assert!(result.is_err(), "expected queue full error");
    assert!(result.unwrap_err().to_string().contains("max_queue_size=2"));
}

#[tokio::test]
async fn permit_drop_releases_slot_on_panic() {
    let q = Arc::new(TaskQueue::new(&config(1, 4)));

    {
        let q_inner = q.clone();
        // Run in a separate task that acquires a permit then panics.
        let handle = tokio::spawn(async move {
            let _permit = q_inner.acquire("proj", 0).await.unwrap();
            panic!("forced panic to test permit drop");
        });
        let _ = handle.await; // ignore the JoinError from the panic
    }

    // The permit must have been released; acquiring again should succeed immediately.
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(result.is_ok(), "permit should be released after panic");
}

#[tokio::test]
async fn queued_count_increments_while_waiting() {
    let q = Arc::new(TaskQueue::new(&config(1, 8)));

    // Hold the slot so the next acquire must wait.
    let _holder = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.queued_count(), 0);

    let q2 = q.clone();
    let _waiter = tokio::spawn(async move { q2.acquire("proj", 0).await });

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(q.queued_count(), 1);
}

#[tokio::test]
async fn running_count_and_queued_count_reset_after_completion() {
    let q = TaskQueue::new(&config(4, 16));
    let p1 = q.acquire("proj", 0).await.unwrap();
    let p2 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
    drop(p1);
    drop(p2);
    assert_eq!(q.running_count(), 0);
    assert_eq!(q.queued_count(), 0);
}

#[tokio::test]
async fn per_project_limit_enforced() {
    use std::collections::HashMap;
    let mut per_project = HashMap::new();
    per_project.insert("proj_a".to_string(), 1usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    // proj_a has limit=1; second acquire must block.
    let _p1 = q.acquire("proj_a", 0).await.unwrap();
    let q2 = q.clone();
    let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a", 0)).await;
    assert!(
        blocked.is_err(),
        "proj_a second acquire should block (limit=1)"
    );

    // proj_b has no configured limit (uses global=4); can still acquire.
    let p_b = q.acquire("proj_b", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);
    drop(p_b);
}

#[tokio::test]
async fn canonical_and_alias_project_keys_share_limit_bucket() -> anyhow::Result<()> {
    use std::collections::HashMap;

    let temp = tempfile::tempdir()?;
    let canonical = temp.path().join("repo");
    std::fs::create_dir_all(&canonical)?;
    let alias = temp.path().join("repo-link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&canonical, &alias)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&canonical, &alias)?;

    let canonical_key = canonical.canonicalize()?.display().to_string();
    let alias_key = alias.display().to_string();

    let mut per_project = HashMap::new();
    per_project.insert(canonical_key, 1usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    let _holder = q.acquire(&alias_key, 0).await.unwrap();
    let blocked = timeout(Duration::from_millis(50), q.acquire(&alias_key, 0)).await;
    assert!(
        blocked.is_err(),
        "alias key should resolve to the canonical per-project limit bucket"
    );
    Ok(())
}

#[tokio::test]
async fn canonical_and_relative_project_keys_share_queue_stats_bucket() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project)?;

    let canonical_key = project.canonicalize()?.display().to_string();
    let relative_key = project.display().to_string();
    let q = Arc::new(TaskQueue::new(&config(1, 8)));

    let _holder = q.acquire(&relative_key, 0).await.unwrap();
    let q2 = q.clone();
    let _waiter = tokio::spawn(async move { q2.acquire(&relative_key, 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let stats = q.project_stats(&canonical_key);
    assert_eq!(stats.running, 1, "canonical key should see running task");
    assert_eq!(stats.queued, 1, "canonical key should see queued waiter");
    Ok(())
}

#[tokio::test]
async fn project_cannot_starve_another() {
    use std::collections::HashMap;
    // global=2, proj_a=1 → proj_a can use at most 1 global slot,
    // leaving at least 1 for proj_b.
    let mut per_project = HashMap::new();
    per_project.insert("proj_a".to_string(), 1usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 2,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    // proj_a fills its project slot (limit=1).
    let _pa = q.acquire("proj_a", 0).await.unwrap();

    // proj_a cannot take another slot even though 1 global slot remains.
    let q2 = q.clone();
    let blocked = timeout(Duration::from_millis(50), q2.acquire("proj_a", 0)).await;
    assert!(
        blocked.is_err(),
        "proj_a blocked at project limit while global slot is free"
    );

    // proj_b can still acquire the remaining global slot.
    let pb = timeout(Duration::from_millis(100), q.acquire("proj_b", 0)).await;
    assert!(pb.is_ok(), "proj_b should get the remaining global slot");
}

#[tokio::test]
async fn project_stats_reflect_running_and_queued() {
    let q = Arc::new(TaskQueue::new(&config(4, 16)));

    let _p1 = q.acquire("stats_proj", 0).await.unwrap();
    let stats = q.project_stats("stats_proj");
    assert_eq!(stats.running, 1);
    assert_eq!(stats.queued, 0);

    // Queue a waiter by capping project limit to 1.
    use std::collections::HashMap;
    let mut per_project = HashMap::new();
    per_project.insert("capped".to_string(), 1usize);
    let cfg2 = ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q2 = Arc::new(TaskQueue::new(&cfg2));
    let _holder = q2.acquire("capped", 0).await.unwrap();
    let q3 = q2.clone();
    let _waiter = tokio::spawn(async move { q3.acquire("capped", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let stats2 = q2.project_stats("capped");
    assert_eq!(stats2.running, 1);
    assert_eq!(stats2.queued, 1);
}

// --- Priority tests ---

#[tokio::test]
async fn high_priority_acquired_before_low_when_slots_full() {
    // 1 slot: hold it, then enqueue priority=0 then priority=1.
    // When the slot is released, priority=1 should unblock first.
    let q = Arc::new(TaskQueue::new(&config(1, 16)));
    let holder = q.acquire("proj", 0).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();

    let q1 = q.clone();
    let tx1 = tx.clone();
    tokio::spawn(async move {
        let _p = q1.acquire("proj", 0).await.unwrap();
        let _ = tx1.send(0);
    });

    // Small delay to ensure priority=0 waiter is registered first.
    tokio::time::sleep(Duration::from_millis(10)).await;

    let q2 = q.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let _p = q2.acquire("proj", 1).await.unwrap();
        let _ = tx2.send(1);
    });

    tokio::time::sleep(Duration::from_millis(10)).await;

    // Release the holder; the priority=1 waiter should unblock first.
    drop(holder);
    tokio::time::sleep(Duration::from_millis(30)).await;

    let first = rx.recv().await.expect("should receive first");
    assert_eq!(first, 1, "priority=1 task should unblock before priority=0");
}

#[tokio::test]
async fn same_priority_is_fifo() {
    // 1 slot: hold it, then enqueue three priority=1 waiters in order.
    // They should unblock in enqueue order (FIFO).
    let q = Arc::new(TaskQueue::new(&config(1, 16)));
    let holder = q.acquire("proj", 1).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u8>();

    for i in 0u8..3 {
        let q_i = q.clone();
        let tx_i = tx.clone();
        tokio::spawn(async move {
            let _p = q_i.acquire("proj", 1).await.unwrap();
            let _ = tx_i.send(i);
            // Hold slot briefly so ordering is observable.
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        // Stagger enqueue so seq ordering is deterministic.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Release the holder.
    drop(holder);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut order = Vec::new();
    while let Ok(v) = rx.try_recv() {
        order.push(v);
    }
    assert_eq!(order, vec![0, 1, 2], "same-priority waiters must be FIFO");
}

#[tokio::test]
async fn priority_zero_default_compiles() {
    let q = TaskQueue::new(&config(2, 8));
    let p = q.acquire("proj", 0).await;
    assert!(p.is_ok());
}

// --- running_count correctness ---

#[tokio::test]
async fn running_count_correct_with_global_waiters() {
    // limit=2; fill both slots, add a waiter in the global queue.
    // running_count must still report 2 — waiters do not reduce available_permits.
    let q = Arc::new(TaskQueue::new(&config(2, 16)));
    let _p1 = q.acquire("proj", 0).await.unwrap();
    let _p2 = q.acquire("proj", 0).await.unwrap();
    assert_eq!(q.running_count(), 2);

    let q2 = q.clone();
    let _h = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert_eq!(
        q.running_count(),
        2,
        "running_count must not decrease because a global waiter is present"
    );
}

// --- Cancellation-safety tests ---

#[tokio::test]
async fn cancelled_project_wait_does_not_leak_queued_count() {
    // 1 project slot; hold it so the next acquire blocks at project-wait.
    use std::collections::HashMap;
    let mut per_project = HashMap::new();
    per_project.insert("proj".to_string(), 1usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));
    let _holder = q.acquire("proj", 0).await.unwrap();

    assert_eq!(q.queued_count(), 0);

    // Spawn an acquire that will block at the project-level wait.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(q.queued_count(), 1);

    // Cancel the waiting future by aborting the task.
    handle.abort();
    assert!(matches!(handle.await, Err(ref e) if e.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(20)).await;

    // queued_count must return to 0 after cancellation.
    assert_eq!(
        q.queued_count(),
        0,
        "queued_count leaked after project-wait cancellation"
    );
}

#[tokio::test]
async fn cancelled_global_wait_releases_project_permit() {
    // project limit=2, global limit=1; hold global so the next acquire
    // passes project-wait then blocks at global-wait.
    use std::collections::HashMap;
    let mut per_project = HashMap::new();
    per_project.insert("proj".to_string(), 2usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 1,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));
    // Hold the single global slot.
    let _global_holder = q.acquire("proj", 0).await.unwrap();

    // This acquire will pass project-wait (limit=2) but block at global-wait.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(q.queued_count(), 1);

    // Cancel the waiting future.
    handle.abort();
    assert!(matches!(handle.await, Err(ref e) if e.is_cancelled()));
    tokio::time::sleep(Duration::from_millis(20)).await;

    // queued_count must return to 0.
    assert_eq!(
        q.queued_count(),
        0,
        "queued_count leaked after global-wait cancellation"
    );

    // The released project permit must allow a new acquisition once the
    // global slot becomes free.
    drop(_global_holder);
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(
        result.is_ok(),
        "project permit not released after global-wait cancellation"
    );
}

// --- Mid-send cancellation race regression tests (Issues 1 & 2) ---

/// Regression: project permit must not be permanently lost when the future is
/// cancelled *after* `release()` sends the signal but *before* `rx.await`
/// completes (the "mid-send" race on the project queue).
#[tokio::test]
async fn project_permit_not_leaked_on_mid_send_cancellation() {
    use std::collections::HashMap;
    let mut per_project = HashMap::new();
    per_project.insert("proj".to_string(), 1usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 4,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    // Hold the single project slot.
    let holder = q.acquire("proj", 0).await.unwrap();

    // Waiter registers in the project queue.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Release holder (sends the permit signal) then immediately cancel the
    // waiter — exercises the race where cancellation happens mid-send.
    drop(holder);
    handle.abort();
    let _ = handle.await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A new task must succeed; the project slot must have been returned.
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(
        result.is_ok(),
        "project permit leaked after mid-send cancellation"
    );
}

/// Regression: global permit must not be permanently lost when the future is
/// cancelled *after* `release()` sends the global signal but *before*
/// `rx.await` completes (the "mid-send" race on the global queue).
#[tokio::test]
async fn global_permit_not_leaked_on_mid_send_cancellation() {
    use std::collections::HashMap;
    let mut per_project = HashMap::new();
    per_project.insert("proj".to_string(), 2usize);
    let cfg = ConcurrencyConfig {
        max_concurrent_tasks: 1,
        max_queue_size: 16,
        stall_timeout_secs: 300,
        per_project,
        ..ConcurrencyConfig::default()
    };
    let q = Arc::new(TaskQueue::new(&cfg));

    // Hold the single global slot.
    let global_holder = q.acquire("proj", 0).await.unwrap();

    // Waiter passes project-wait (project limit=2) and blocks at global-wait.
    let q2 = q.clone();
    let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Release global holder (sends the global signal) then cancel the waiter.
    drop(global_holder);
    handle.abort();
    let _ = handle.await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A new task must succeed; the global slot must have been returned.
    let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
    assert!(
        result.is_ok(),
        "global permit leaked after mid-send cancellation"
    );
}

#[tokio::test]
async fn task_admission_blocked_under_pressure() {
    let pressure = Arc::new(AtomicBool::new(true));
    let q = TaskQueue::new_with_pressure(&config(4, 16), Some(pressure));
    let result = q.acquire("proj", 0).await;
    assert!(result.is_err(), "acquire must fail under memory pressure");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("available system memory"),
        "error message should mention memory"
    );
}

#[tokio::test]
async fn task_admission_succeeds_no_pressure() {
    let pressure = Arc::new(AtomicBool::new(false));
    let q = TaskQueue::new_with_pressure(&config(4, 16), Some(pressure));
    let result = q.acquire("proj", 0).await;
    assert!(
        result.is_ok(),
        "acquire must succeed when pressure flag is false"
    );
}
