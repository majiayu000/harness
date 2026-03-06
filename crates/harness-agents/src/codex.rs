use crate::streaming::send_stream_item;
use async_trait::async_trait;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, Item, StreamItem, TokenUsage,
};
use std::path::PathBuf;
use tokio::process::Command;

pub struct CodexAgent {
    pub cli_path: PathBuf,
}

impl CodexAgent {
    pub fn new(cli_path: PathBuf) -> Self {
        Self { cli_path }
    }
}

#[async_trait]
impl CodeAgent for CodexAgent {
    fn name(&self) -> &str {
        "codex"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("exec")
            .arg("--skip-git-repo-check")
            .arg("-s")
            .arg("read-only")
            .arg("-C")
            .arg(&req.project_root)
            .arg(&req.prompt);

        let output = cmd.output().await.map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to run codex: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(harness_core::HarnessError::AgentExecution(format!(
                "codex exited with {}: {stderr}",
                output.status
            )));
        }

        Ok(AgentResponse {
            output: stdout,
            stderr,
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: "codex".to_string(),
            exit_code: output.status.code(),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::Result<()> {
        let resp = self.execute(req).await?;
        send_stream_item(
            &tx,
            StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: resp.output.clone(),
                },
            },
            self.name(),
            "item_completed",
        )
        .await?;
        send_stream_item(&tx, StreamItem::Done, self.name(), "done").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_stream_returns_error_when_channel_closed() {
        let agent = CodexAgent::new(PathBuf::from("/usr/bin/true"));
        let request = AgentRequest::default();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);

        let err = agent
            .execute_stream(request, tx)
            .await
            .expect_err("execute_stream should fail when receiver is dropped");

        let message = err.to_string();
        assert!(
            message.contains("stream send failed"),
            "expected send failure in error message, got: {message}"
        );
    }
}
