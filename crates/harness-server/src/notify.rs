use harness_protocol::{notifications::Notification, notifications::RpcNotification};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

pub type NotifySender = mpsc::Sender<RpcNotification>;
pub type NotifyReceiver = mpsc::Receiver<RpcNotification>;

const DROP_LOG_EVERY: u64 = 100;
static DROPPED_NOTIFICATIONS: AtomicU64 = AtomicU64::new(0);

/// Create a bounded channel for server-push notifications.
pub fn channel(capacity: usize) -> (NotifySender, NotifyReceiver) {
    mpsc::channel(capacity)
}

/// Emit a notification fire-and-forget style.
/// Drops and records the notification if the channel is full or closed.
pub fn emit(tx: &Option<NotifySender>, notification: Notification) {
    if let Some(tx) = tx {
        if let Err(err) = tx.try_send(RpcNotification::new(notification)) {
            match err {
                mpsc::error::TrySendError::Full(_) => record_drop("full"),
                mpsc::error::TrySendError::Closed(_) => record_drop("closed"),
            }
        }
    }
}

fn record_drop(reason: &'static str) {
    let dropped_count = DROPPED_NOTIFICATIONS.fetch_add(1, Ordering::Relaxed) + 1;
    if dropped_count == 1 || dropped_count % DROP_LOG_EVERY == 0 {
        tracing::warn!(
            event = "notify_channel_drop",
            reason,
            dropped_count,
            "dropping outbound notification"
        );
    }
}

#[cfg(test)]
fn dropped_notification_count() -> u64 {
    DROPPED_NOTIFICATIONS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{types::ThreadId, types::ThreadStatus};

    #[tokio::test]
    async fn emit_sends_to_channel() {
        let (tx, mut rx) = channel(8);
        let opt = Some(tx);
        let thread_id = ThreadId::new();

        emit(
            &opt,
            Notification::ThreadStatusChanged {
                thread_id: thread_id.clone(),
                status: ThreadStatus::Idle,
            },
        );

        let received = rx.recv().await.expect("notification should be received");
        assert_eq!(received.jsonrpc, "2.0");
        let json = serde_json::to_string(&received).unwrap();
        assert!(
            json.contains("thread/status_changed"),
            "unexpected method: {json}"
        );
    }

    #[tokio::test]
    async fn emit_noop_when_none() {
        // Should not panic.
        emit(
            &None,
            Notification::ThreadStatusChanged {
                thread_id: ThreadId::new(),
                status: ThreadStatus::Idle,
            },
        );
    }

    #[tokio::test]
    async fn emit_drops_when_channel_full() {
        let (tx, _rx) = channel(1);
        let opt = Some(tx);
        let dropped_before = dropped_notification_count();
        // Fill channel.
        emit(
            &opt,
            Notification::ThreadStatusChanged {
                thread_id: ThreadId::new(),
                status: ThreadStatus::Idle,
            },
        );
        // Should not panic even though channel is full.
        emit(
            &opt,
            Notification::ThreadStatusChanged {
                thread_id: ThreadId::new(),
                status: ThreadStatus::Active,
            },
        );
        let dropped_after = dropped_notification_count();
        assert!(
            dropped_after > dropped_before,
            "expected drop count to increase when channel is full"
        );
    }

    #[tokio::test]
    async fn emit_tracks_drops_in_pressure_burst() {
        let (tx, _rx) = channel(1);
        let opt = Some(tx);
        let dropped_before = dropped_notification_count();
        for _ in 0..256 {
            emit(
                &opt,
                Notification::ThreadStatusChanged {
                    thread_id: ThreadId::new(),
                    status: ThreadStatus::Active,
                },
            );
        }
        let dropped_after = dropped_notification_count();
        assert!(
            dropped_after.saturating_sub(dropped_before) >= 255,
            "expected at least 255 dropped notifications, got {}",
            dropped_after.saturating_sub(dropped_before)
        );
    }
}
