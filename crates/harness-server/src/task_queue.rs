use harness_core::{config::ConcurrencyConfig, HarnessError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Acquired execution slot. Dropping this releases the slot, even on panic.
#[derive(Debug)]
pub struct TaskPermit {
    _permit: OwnedSemaphorePermit,
}

/// Bounded task queue using a semaphore for concurrency control.
///
/// Tasks wait in Pending status until an execution slot opens. If the number
/// of tasks already waiting exceeds `max_queue_size`, new submissions are
/// rejected immediately.
pub struct TaskQueue {
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    max_queue_size: usize,
    queued_count: AtomicUsize,
}

impl TaskQueue {
    pub fn new(config: &ConcurrencyConfig) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(config.max_concurrent_tasks)),
            max_concurrent: config.max_concurrent_tasks,
            max_queue_size: config.max_queue_size,
            queued_count: AtomicUsize::new(0),
        }
    }

    /// Acquire an execution slot. Blocks until a slot is available.
    ///
    /// Returns `Err` immediately if `max_queue_size` tasks are already waiting.
    /// The returned permit releases its slot on drop (even on panic).
    pub async fn acquire(&self) -> harness_core::Result<TaskPermit> {
        // Reserve a queue slot atomically. fetch_add returns the value before
        // the increment, so if prev == max_queue_size the queue is already full.
        let prev = self.queued_count.fetch_add(1, Ordering::SeqCst);
        if prev >= self.max_queue_size {
            self.queued_count.fetch_sub(1, Ordering::SeqCst);
            return Err(HarnessError::Other(format!(
                "task queue is full (max_queue_size={})",
                self.max_queue_size
            )));
        }

        // Block until an execution slot opens.
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| HarnessError::Other("task queue closed".to_string()))?;

        // Slot acquired; decrement the waiting count.
        self.queued_count.fetch_sub(1, Ordering::SeqCst);

        Ok(TaskPermit { _permit: permit })
    }

    /// Number of tasks currently holding an execution slot (running).
    pub fn running_count(&self) -> usize {
        self.max_concurrent
            .saturating_sub(self.semaphore.available_permits())
    }

    /// Number of tasks waiting for an execution slot.
    pub fn queued_count(&self) -> usize {
        self.queued_count.load(Ordering::Relaxed)
    }

    /// Create an unbounded queue for tests (effectively no limit).
    #[cfg(test)]
    pub fn unbounded() -> Self {
        Self::new(&ConcurrencyConfig {
            max_concurrent_tasks: 1024,
            max_queue_size: 1024,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    fn config(max_concurrent: usize, max_queue: usize) -> ConcurrencyConfig {
        ConcurrencyConfig {
            max_concurrent_tasks: max_concurrent,
            max_queue_size: max_queue,
        }
    }

    #[tokio::test]
    async fn acquire_and_release_single_permit() {
        let q = TaskQueue::new(&config(2, 8));
        assert_eq!(q.running_count(), 0);
        let permit = q.acquire().await.unwrap();
        assert_eq!(q.running_count(), 1);
        drop(permit);
        assert_eq!(q.running_count(), 0);
    }

    #[tokio::test]
    async fn only_max_concurrent_tasks_run_simultaneously() {
        let q = Arc::new(TaskQueue::new(&config(2, 16)));

        // Acquire both permits.
        let p1 = q.acquire().await.unwrap();
        let p2 = q.acquire().await.unwrap();
        assert_eq!(q.running_count(), 2);

        // Third acquire must block; verify with a short timeout.
        let q2 = q.clone();
        let blocked = timeout(Duration::from_millis(50), q2.acquire()).await;
        assert!(blocked.is_err(), "third acquire should block when limit=2");

        // Release a slot; third acquire should now succeed.
        drop(p1);
        let p3 = q.acquire().await.unwrap();
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
        let _p1 = q.acquire().await.unwrap();

        // Two tasks queue up (they block, so spawn them).
        let q2 = q.clone();
        let _h1 = tokio::spawn(async move { q2.acquire().await });
        let q3 = q.clone();
        let _h2 = tokio::spawn(async move { q3.acquire().await });

        // Give spawned tasks time to increment queued_count.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Third waiter exceeds max_queue_size=2 → error.
        let result = q.acquire().await;
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
                let _permit = q_inner.acquire().await.unwrap();
                panic!("forced panic to test permit drop");
            });
            let _ = handle.await; // ignore the JoinError from the panic
        }

        // The permit must have been released; acquiring again should succeed immediately.
        let result = timeout(Duration::from_millis(100), q.acquire()).await;
        assert!(result.is_ok(), "permit should be released after panic");
    }

    #[tokio::test]
    async fn queued_count_increments_while_waiting() {
        let q = Arc::new(TaskQueue::new(&config(1, 8)));

        // Hold the slot so the next acquire must wait.
        let _holder = q.acquire().await.unwrap();
        assert_eq!(q.queued_count(), 0);

        let q2 = q.clone();
        let _waiter = tokio::spawn(async move { q2.acquire().await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(q.queued_count(), 1);
    }

    #[tokio::test]
    async fn running_count_and_queued_count_reset_after_completion() {
        let q = TaskQueue::new(&config(4, 16));
        let p1 = q.acquire().await.unwrap();
        let p2 = q.acquire().await.unwrap();
        assert_eq!(q.running_count(), 2);
        drop(p1);
        drop(p2);
        assert_eq!(q.running_count(), 0);
        assert_eq!(q.queued_count(), 0);
    }
}
