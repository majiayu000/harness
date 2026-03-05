use harness_protocol::{Notification, RpcNotification};
use tokio::sync::mpsc;

pub type NotifySender = mpsc::Sender<RpcNotification>;
pub type NotifyReceiver = mpsc::Receiver<RpcNotification>;

/// Create a bounded channel for server-push notifications.
pub fn channel(capacity: usize) -> (NotifySender, NotifyReceiver) {
    mpsc::channel(capacity)
}

/// Emit a notification fire-and-forget style.
/// Silently drops the notification if the channel is full or closed.
pub fn emit(tx: &Option<NotifySender>, notification: Notification) {
    if let Some(tx) = tx {
        let _ = tx.try_send(RpcNotification::new(notification));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{ThreadId, ThreadStatus};

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
            json.contains("thread_status_changed"),
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
    }
}
