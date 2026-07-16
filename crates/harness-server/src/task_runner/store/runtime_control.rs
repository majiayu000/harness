use super::*;

impl TaskStore {
    /// Register a broadcast channel for a task's stream. Call once before task execution starts.
    pub(crate) fn register_task_stream(&self, id: &TaskId) {
        let (tx, _rx) = broadcast::channel(TASK_STREAM_CAPACITY);
        self.stream_txs.insert(id.clone(), tx);
    }

    /// Subscribe to a task's active stream. Returns `None` if no stream is registered.
    pub fn subscribe_task_stream(&self, id: &TaskId) -> Option<broadcast::Receiver<StreamItem>> {
        record_task_runner_usage();
        self.stream_txs.get(id).map(|tx| tx.subscribe())
    }

    /// Publish a [`StreamItem`] to all current subscribers of a task stream.
    /// No-op when no stream is registered. Dropping the send result is intentional:
    /// `SendError` only occurs when there are no active receivers, which is normal
    /// when no SSE client is connected (backpressure: oldest events dropped on lag).
    pub(crate) fn publish_stream_item(&self, id: &TaskId, item: StreamItem) {
        if let Some(tx) = self.stream_txs.get(id) {
            if let Err(e) = tx.send(item) {
                tracing::trace!(task_id = %id.0, "stream publish dropped (no receivers): {e}");
            }
        }
    }

    /// Remove the task's stream channel after execution completes.
    pub(crate) fn close_task_stream(&self, id: &TaskId) {
        self.stream_txs.remove(id);
    }

    /// Store the abort handle for a running task so it can be cancelled later.
    pub(crate) fn store_abort_handle(&self, id: &TaskId, handle: tokio::task::AbortHandle) {
        self.abort_handles.insert(id.clone(), handle);
    }

    /// Remove and discard the abort handle when the task finishes normally.
    pub(crate) fn remove_abort_handle(&self, id: &TaskId) {
        self.abort_handles.remove(id);
    }

    /// Abort the running task's Tokio future, if one is registered.
    /// The `kill_on_drop(true)` flag on child processes ensures the CLI subprocess
    /// is also killed when the future is dropped after abort.
    /// Returns `true` if an abort handle was found and triggered.
    pub fn abort_task(&self, id: &TaskId) -> bool {
        record_task_runner_usage();
        if let Some(handle) = self.abort_handles.get(id) {
            handle.abort();
            true
        } else {
            false
        }
    }

    /// Activate the global rate-limit circuit breaker. All tasks will pause
    /// before their next agent call until `duration` elapses.
    pub async fn set_rate_limit(&self, duration: std::time::Duration) {
        record_task_runner_usage();
        let until = tokio::time::Instant::now() + duration;
        *self.rate_limit_until.write().await = Some(until);
        tracing::warn!(
            pause_secs = duration.as_secs(),
            "rate-limit circuit breaker activated — all tasks paused"
        );
    }

    /// Wait if the global rate-limit circuit breaker is active.
    /// Returns only after the deadline is None or already past. Loops to handle
    /// the case where a concurrent `set_rate_limit()` extends the deadline while
    /// this task is sleeping — without looping, the task would proceed after the
    /// original deadline and burn turns / trigger extra 429s.
    pub async fn wait_for_rate_limit(&self) {
        record_task_runner_usage();
        loop {
            let deadline = { *self.rate_limit_until.read().await };
            let Some(until) = deadline else { return };
            let now = tokio::time::Instant::now();
            if now >= until {
                // Deadline already passed. Clear it if unchanged; if a concurrent
                // setter extended the deadline, re-loop to wait for the new value.
                let mut wl = self.rate_limit_until.write().await;
                if *wl == Some(until) {
                    *wl = None;
                    return;
                }
                // Deadline was extended concurrently — fall through to re-loop.
                continue;
            }
            let remaining = until - now;
            tracing::info!(
                remaining_secs = remaining.as_secs(),
                "task waiting for rate-limit circuit breaker to clear"
            );
            tokio::time::sleep_until(until).await;
            // Only clear the breaker if it still holds the same deadline we
            // snapshotted. A concurrent set_rate_limit() call may have extended
            // the deadline during our sleep; preserving the new deadline lets
            // the next loop iteration wait for the extension.
            {
                let mut wl = self.rate_limit_until.write().await;
                if *wl == Some(until) {
                    *wl = None;
                }
            }
            // Loop again: re-read the deadline in case it was extended.
        }
    }
}
