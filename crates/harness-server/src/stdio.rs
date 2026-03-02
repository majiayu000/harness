use crate::server::HarnessServer;
use crate::router;
use harness_protocol::{codec, RpcResponse};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Serve JSON-RPC over stdio (one JSON object per line).
pub async fn serve(server: &HarnessServer) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    tracing::info!("harness: stdio server started");

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match codec::decode_request(&line) {
            Ok(req) => router::handle_request(server, req).await,
            Err(e) => RpcResponse::error(
                None,
                harness_protocol::PARSE_ERROR,
                format!("parse error: {e}"),
            ),
        };

        let out = codec::encode_response(&response)?;
        stdout.write_all(out.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}
