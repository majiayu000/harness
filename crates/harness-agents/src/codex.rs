use crate::streaming::send_stream_item;
use async_trait::async_trait;
use harness_core::SandboxMode;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, Item, StreamItem, TokenUsage,
};
use harness_sandbox::{wrap_command, SandboxSpec};
use std::ffi::OsString;
use std::path::PathBuf;
use tokio::process::Command;

pub struct CodexAgent {
    pub cli_path: PathBuf,
    pub sandbox_mode: SandboxMode,
}

impl CodexAgent {
    pub fn new(cli_path: PathBuf, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path,
            sandbox_mode,
        }
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
        let base_args = vec![
            OsString::from("exec"),
            OsString::from("--skip-git-repo-check"),
            OsString::from("-a"),
            OsString::from(self.sandbox_mode.to_string()),
            OsString::from("-C"),
            req.project_root.as_os_str().to_os_string(),
            OsString::from(req.prompt.clone()),
        ];

        let sandbox_spec = SandboxSpec::new(self.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for codex: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root);

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
        let agent = CodexAgent::new(
            PathBuf::from("/usr/bin/true"),
            SandboxMode::DangerFullAccess,
        );
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
