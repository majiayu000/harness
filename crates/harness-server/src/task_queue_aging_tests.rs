//! Priority aging (anti-starvation) tests for [`super::TaskQueue`] —
//! GH-1583 invariants B-001..B-008. All tests use the paused tokio clock
//! (`start_paused = true`) so aging is fully deterministic.
// The enclosing `mod aging_tests` is already declared `#[cfg(test)]` in
// task_queue.rs; the inner `#[cfg(test)] mod cases` marker keeps the file
// self-describing as test-only code.
#[cfg(test)]
mod cases {
    use super::super::*;
    use harness_core::config::misc::PriorityAgingConfig;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Config with aging enabled at a short, test-friendly interval.
    fn aging_config(
        max_concurrent: usize,
        max_queue: usize,
        interval_secs: u64,
    ) -> ConcurrencyConfig {
        ConcurrencyConfig {
            max_concurrent_tasks: max_concurrent,
            max_queue_size: max_queue,
            stall_timeout_secs: 300,
            per_project: Default::default(),
            aging: PriorityAgingConfig {
                enabled: true,
                interval_secs,
                max_boost_levels: 2,
            },
            ..ConcurrencyConfig::default()
        }
    }

    /// Spawn a task that acquires with `priority`, reports `label` once granted,
    /// then holds the permit until the returned sender is signalled or dropped.
    fn spawn_holder(
        q: &Arc<TaskQueue>,
        priority: u8,
        label: &'static str,
        granted_tx: tokio::sync::mpsc::UnboundedSender<&'static str>,
    ) -> tokio::sync::oneshot::Sender<()> {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let q = q.clone();
        tokio::spawn(async move {
            let _p = q.acquire("proj", priority).await.unwrap();
            let _ = granted_tx.send(label);
            let _ = release_rx.await;
        });
        release_tx
    }

    /// B-001: effective priority rises one level per interval, capped at
    /// `base + max_boost_levels` and never above `MAX_PRIORITY_LEVEL`.
    #[tokio::test(start_paused = true)]
    async fn effective_priority_boosts_per_interval_and_caps() {
        use super::super::permits::{AgingParams, PriorityPermitQueue};
        let aging = AgingParams::from_config(&PriorityAgingConfig {
            enabled: true,
            interval_secs: 10,
            max_boost_levels: 2,
        });
        // Zero capacity so every try_acquire enqueues a waiter.
        let mut q = PriorityPermitQueue::new(0, aging);
        let _w0 = q.try_acquire_or_enqueue(0).unwrap_err(); // index 0, base 0
        let _w1 = q.try_acquire_or_enqueue(1).unwrap_err(); // index 1, base 1

        let now = Instant::now();
        assert_eq!(q.effective_priority_at(0, now), 0);
        assert_eq!(q.effective_priority_at(1, now), 1);

        tokio::time::advance(Duration::from_secs(10)).await;
        let now = Instant::now();
        assert_eq!(q.effective_priority_at(0, now), 1, "+1 after one interval");
        assert_eq!(q.effective_priority_at(1, now), 2, "+1 after one interval");

        tokio::time::advance(Duration::from_secs(10)).await;
        let now = Instant::now();
        assert_eq!(q.effective_priority_at(0, now), 2, "+2 after two intervals");
        assert_eq!(
            q.effective_priority_at(1, now),
            2,
            "never above MAX_PRIORITY_LEVEL"
        );

        tokio::time::advance(Duration::from_secs(1000)).await;
        let now = Instant::now();
        assert_eq!(q.effective_priority_at(0, now), 2, "cap base + 2 must hold");
        assert_eq!(q.effective_priority_at(1, now), 2, "ceiling must hold");
    }

    /// B-002 + B-004: under a steady stream of max-priority arrivals, a priority-0
    /// waiter is granted within `max_boost_levels * interval` plus one FIFO turn —
    /// once aged to the cap it wins ties against fresh max-priority arrivals by
    /// enqueue sequence.
    #[tokio::test(start_paused = true)]
    async fn aged_low_priority_not_starved_beyond_bound() {
        // Single slot, aging: +1 level per 5s, cap +2.
        let q = Arc::new(TaskQueue::new(&aging_config(1, 16, 5)));
        let holder = q.acquire("proj", 2).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();

        // The starving low-priority waiter enqueues first.
        let _low_release = spawn_holder(&q, 0, "low", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Turn 1: a fresh max-priority arrival beats the un-aged waiter.
        let h1_release = spawn_holder(&q, 2, "h1", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(holder);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(rx.recv().await, Some("h1"), "fresh p2 beats un-aged p0");

        // Turn 2: after one interval the waiter is at effective 1 — still loses.
        tokio::time::advance(Duration::from_secs(5)).await;
        let h2_release = spawn_holder(&q, 2, "h2", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = h1_release.send(());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(rx.recv().await, Some("h2"), "p2 still beats effective-1 p0");

        // Turn 3: after two intervals the waiter is capped at effective 2 and
        // beats any fresh max-priority arrival by enqueue sequence (B-004).
        tokio::time::advance(Duration::from_secs(5)).await;
        let _h3_release = spawn_holder(&q, 2, "h3", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = h2_release.send(());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            rx.recv().await,
            Some("low"),
            "aged p0 must win the tie against a fresh p2 (bound: 2 intervals + one turn)"
        );
    }

    /// B-003: with aging disabled, grant order is exactly (priority DESC, FIFO)
    /// no matter how long a low-priority waiter has been queued.
    #[tokio::test(start_paused = true)]
    async fn aging_off_matches_legacy_order() {
        let mut cfg = aging_config(1, 16, 5);
        cfg.aging.enabled = false;
        let q = Arc::new(TaskQueue::new(&cfg));
        let holder = q.acquire("proj", 1).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();

        let low_release = spawn_holder(&q, 0, "low", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Wait far longer than any aging interval could boost.
        tokio::time::advance(Duration::from_secs(3600)).await;

        let high_release = spawn_holder(&q, 1, "high", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        drop(holder);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            rx.recv().await,
            Some("high"),
            "with aging off, an hour-old p0 must not outrank a fresh p1"
        );
        let _ = high_release.send(());
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(rx.recv().await, Some("low"));
        drop(low_release);
    }

    /// B-004 (focused): a tie in effective priority created by aging resolves to
    /// the earlier enqueue sequence deterministically.
    #[tokio::test(start_paused = true)]
    async fn aged_tie_breaks_by_enqueue_order() {
        let q = Arc::new(TaskQueue::new(&aging_config(1, 16, 5)));
        let holder = q.acquire("proj", 2).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();

        // p0 enqueues first, then ages one level to tie a fresh p1.
        let _aged_release = spawn_holder(&q, 0, "aged_p0", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;
        tokio::time::advance(Duration::from_secs(5)).await;

        let _fresh_release = spawn_holder(&q, 1, "fresh_p1", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        drop(holder);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            rx.recv().await,
            Some("aged_p0"),
            "aged tie must break by enqueue sequence (earlier waiter wins)"
        );
    }

    /// B-006: a waiter cancelled mid-wait is never granted by aging (even when
    /// aged to the cap), leaks no permit, and is excluded from wait-time metrics.
    #[tokio::test(start_paused = true)]
    async fn cancelled_aged_waiter_never_granted_and_excluded_from_metrics() {
        let q = Arc::new(TaskQueue::new(&aging_config(1, 16, 5)));
        let holder = q.acquire("proj", 0).await.unwrap();

        let q2 = q.clone();
        let handle = tokio::spawn(async move { q2.acquire("proj", 0).await });
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Age the waiter to the cap, then cancel it.
        tokio::time::advance(Duration::from_secs(20)).await;
        handle.abort();
        assert!(matches!(handle.await, Err(ref e) if e.is_cancelled()));
        tokio::time::sleep(Duration::from_millis(10)).await;

        drop(holder);
        tokio::time::sleep(Duration::from_millis(10)).await;

        // The permit must be free for a new acquire (cancelled waiter not granted).
        let result = timeout(Duration::from_millis(100), q.acquire("proj", 0)).await;
        assert!(
            result.is_ok(),
            "permit leaked after aged-waiter cancellation"
        );

        // The cancelled waiter must not appear in wait-time metrics.
        let stats = q.project_stats("proj");
        let level0 = &stats.wait_ms_by_priority[0];
        assert_eq!(level0.priority, 0);
        assert_eq!(
            level0.granted, 0,
            "cancelled waiter must be excluded from grant metrics"
        );
    }

    /// B-007: aging applies independently at the global stage (the project stage
    /// is exercised by `aged_low_priority_not_starved_beyond_bound`); the
    /// per-stage bound holds when contention is at the global queue.
    #[tokio::test(start_paused = true)]
    async fn aging_bound_holds_at_global_stage() {
        use std::collections::HashMap;
        // Project limit high, global limit 1 → all waiting happens at the global stage.
        let mut per_project = HashMap::new();
        per_project.insert("proj".to_string(), 8usize);
        let mut cfg = aging_config(1, 16, 5);
        cfg.per_project = per_project;
        let q = Arc::new(TaskQueue::new(&cfg));
        let holder = q.acquire("proj", 2).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();

        // Low-priority waiter passes the project stage and parks at the global stage.
        let _low_release = spawn_holder(&q, 0, "low", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Age to the cap at the global stage.
        tokio::time::advance(Duration::from_secs(10)).await;

        // A fresh max-priority arrival also parks at the global stage.
        let _high_release = spawn_holder(&q, 2, "high", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        drop(holder);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            rx.recv().await,
            Some("low"),
            "global-stage aging must grant the capped p0 waiter over a fresh p2"
        );
    }

    /// B-008: granted waiters produce per-priority-level wait metrics (grant
    /// count, max, p95) on the project-stage stats and the global-stage diagnostics.
    #[tokio::test(start_paused = true)]
    async fn wait_time_metrics_recorded_per_priority_level() {
        let q = Arc::new(TaskQueue::new(&aging_config(1, 16, 300)));
        let holder = q.acquire("proj", 0).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<&'static str>();
        let _w_release = spawn_holder(&q, 1, "w", tx.clone());
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Let the waiter wait ~2s, then grant it.
        tokio::time::advance(Duration::from_secs(2)).await;
        drop(holder);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(rx.recv().await, Some("w"));

        let stats = q.project_stats("proj");
        assert_eq!(stats.wait_ms_by_priority.len(), 3, "levels 0..=2");
        let level1 = &stats.wait_ms_by_priority[1];
        assert_eq!(level1.priority, 1);
        assert_eq!(level1.granted, 1, "one granted waiter at base priority 1");
        assert!(
            level1.max_wait_ms >= 2000,
            "max wait must reflect the ~2s wait, got {}",
            level1.max_wait_ms
        );
        assert!(
            level1.p95_wait_ms >= 2000,
            "p95 must reflect the ~2s wait, got {}",
            level1.p95_wait_ms
        );
        // Immediate grants (the holder) never queued and record no sample.
        assert_eq!(stats.wait_ms_by_priority[0].granted, 0);

        // The global stage never had waiters here, but the diagnostics surface
        // must expose the same per-level shape.
        let diag = q.diagnostics("proj");
        assert_eq!(diag.global_wait_ms_by_priority.len(), 3);
        assert!(diag
            .global_wait_ms_by_priority
            .iter()
            .all(|l| l.granted == 0));
    }
}
