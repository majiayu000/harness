use harness_core::{HarnessError, StreamItem};
use tokio::sync::mpsc::Sender;

pub(crate) async fn send_stream_item(
    tx: &Sender<StreamItem>,
    item: StreamItem,
    agent_name: &str,
    item_label: &'static str,
) -> harness_core::Result<()> {
    tx.send(item).await.map_err(|err| {
        tracing::error!(
            agent = agent_name,
            stream_item = item_label,
            error = %err,
            "failed to send stream item"
        );
        HarnessError::AgentExecution(format!(
            "{agent_name} stream send failed while sending {item_label}: {err}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::HarnessError;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn send_stream_item_reports_closed_channel_with_context() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let error = send_stream_item(&tx, StreamItem::Done, "anthropic-api", "done")
            .await
            .expect_err("closed channel should return an error");

        match error {
            HarnessError::AgentExecution(message) => {
                assert!(message.contains("anthropic-api stream send failed while sending done"));
            }
            other => panic!("expected HarnessError::AgentExecution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_stream_item_succeeds_when_channel_open() {
        let (tx, mut rx) = mpsc::channel(1);
        send_stream_item(&tx, StreamItem::Done, "anthropic-api", "done")
            .await
            .expect("open channel should send successfully");

        let received = rx.recv().await;
        assert!(matches!(received, Some(StreamItem::Done)));
    }
}
