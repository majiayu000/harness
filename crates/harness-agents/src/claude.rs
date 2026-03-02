use async_trait::async_trait;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, Item, StreamItem, TokenUsage,
};
use std::path::PathBuf;
use tokio::process::Command;

pub struct ClaudeCodeAgent {
    pub cli_path: PathBuf,
    pub default_model: String,
}

impl ClaudeCodeAgent {
    pub fn new(cli_path: PathBuf, default_model: String) -> Self {
        Self {
            cli_path,
            default_model,
        }
    }
}

#[async_trait]
impl CodeAgent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        "claude"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability::Read,
            Capability::Write,
            Capability::Execute,
        ]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::Result<AgentResponse> {
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("-p")
            .arg("--dangerously-skip-permissions")
            .arg("--output-format").arg("text")
            .arg("--model").arg(model)
            .arg("--verbose")
            .current_dir(&req.project_root)
            .env_remove("CLAUDECODE");

        if !req.allowed_tools.is_empty() {
            cmd.arg("--allowedTools").arg(req.allowed_tools.join(","));
        }

        if let Some(budget) = req.max_budget_usd {
            cmd.arg("--max-budget-usd").arg(budget.to_string());
        }

        cmd.arg(&req.prompt);

        let output = cmd.output().await.map_err(|e| {
            harness_core::HarnessError::AgentExecution(format!("failed to run claude: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !stderr.is_empty() {
            eprintln!("[agent:claude] stderr:\n{stderr}");
        }

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
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: resp.output.clone(),
                },
            })
            .await;
        let _ = tx
            .send(StreamItem::TokenUsage {
                usage: resp.token_usage,
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}
