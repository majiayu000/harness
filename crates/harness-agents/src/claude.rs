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

pub struct ClaudeCodeAgent {
    pub cli_path: PathBuf,
    pub default_model: String,
    pub sandbox_mode: SandboxMode,
}

impl ClaudeCodeAgent {
    pub fn new(cli_path: PathBuf, default_model: String, sandbox_mode: SandboxMode) -> Self {
        Self {
            cli_path,
            default_model,
            sandbox_mode,
        }
    }
}

#[async_trait]
impl CodeAgent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        "claude"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write, Capability::Execute]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut base_args = vec![
            OsString::from("-p"),
            OsString::from("--dangerously-skip-permissions"),
            OsString::from("--output-format"),
            OsString::from("text"),
            OsString::from("--model"),
            OsString::from(model),
            OsString::from("--verbose"),
        ];

        if !req.allowed_tools.is_empty() {
            base_args.push(OsString::from("--allowedTools"));
            base_args.push(OsString::from(req.allowed_tools.join(",")));
        }

        if let Some(budget) = req.max_budget_usd {
            base_args.push(OsString::from("--max-budget-usd"));
            base_args.push(OsString::from(budget.to_string()));
        }

        base_args.push(OsString::from(req.prompt.clone()));

        let sandbox_spec = SandboxSpec::new(self.sandbox_mode, &req.project_root);
        let wrapped_command =
            wrap_command(&self.cli_path, &base_args, &sandbox_spec).map_err(|error| {
                harness_core::HarnessError::AgentExecution(format!(
                    "sandbox setup failed for claude: {error}"
                ))
            })?;

        let mut cmd = Command::new(&wrapped_command.program);
        cmd.args(&wrapped_command.args)
            .current_dir(&req.project_root)
            .env_remove("CLAUDECODE");

        let output = cmd.output().await.map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to run claude: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(harness_core::HarnessError::AgentExecution(format!(
                "claude exited with {}: {stderr}",
                output.status
            )));
        }

        Ok(AgentResponse {
            output: stdout,
            stderr,
            items: Vec::new(),
            token_usage: TokenUsage::default(),
            model: model.to_string(),
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
        send_stream_item(
            &tx,
            StreamItem::TokenUsage {
                usage: resp.token_usage,
            },
            self.name(),
            "token_usage",
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
        let agent = ClaudeCodeAgent::new(
            PathBuf::from("/usr/bin/true"),
            "test-model".to_string(),
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
