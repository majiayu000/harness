use harness_core::{HarnessError, Item, StreamItem};
use tokio::io::{AsyncReadExt, BufReader};
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

/// Read stdout from a spawned child process, stream deltas via `tx`,
/// wait for exit, and return the collected output.
pub(crate) async fn stream_child_output(
    child: &mut tokio::process::Child,
    tx: &Sender<StreamItem>,
    agent_name: &str,
) -> harness_core::Result<String> {
    let stdout = child.stdout.take().ok_or_else(|| {
        HarnessError::AgentExecution(format!("{agent_name} stdout unavailable"))
    })?;

    let mut reader = BufReader::new(stdout);
    let mut output = String::new();
    let mut chunk = [0_u8; 1024];

    loop {
        let read = reader.read(&mut chunk).await.map_err(|error| {
            HarnessError::AgentExecution(format!(
                "failed reading {agent_name} stdout: {error}"
            ))
        })?;
        if read == 0 {
            break;
        }

        let delta = String::from_utf8_lossy(&chunk[..read]).to_string();
        output.push_str(&delta);
        send_stream_item(
            tx,
            StreamItem::MessageDelta { text: delta },
            agent_name,
            "message_delta",
        )
        .await?;
    }

    let status = child.wait().await.map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed waiting for {agent_name} process: {error}"
        ))
    })?;
    if !status.success() {
        return Err(HarnessError::AgentExecution(format!(
            "{agent_name} exited with {status}"
        )));
    }

    send_stream_item(
        tx,
        StreamItem::ItemCompleted {
            item: Item::AgentReasoning {
                content: output.clone(),
            },
        },
        agent_name,
        "item_completed",
    )
    .await?;

    Ok(output)
}
