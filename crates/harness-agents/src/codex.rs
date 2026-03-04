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

    fn build_exec_command(&self, req: &AgentRequest) -> Command {
        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("exec")
            .arg("--skip-git-repo-check")
            .arg("--full-auto")
            .arg("-C")
            .arg(&req.project_root);

        if let Some(model) = req.model.as_deref() {
            cmd.arg("-m").arg(model);
        }

        cmd.arg(&req.prompt);
        cmd
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
        let output = self.build_exec_command(&req).output().await.map_err(|e| {
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
        let _ = tx
            .send(StreamItem::ItemCompleted {
                item: Item::AgentReasoning {
                    content: resp.output.clone(),
                },
            })
            .await;
        let _ = tx.send(StreamItem::Done).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn codex_exec_uses_full_auto_workspace_write_mode() {
        let agent = CodexAgent::new(PathBuf::from("codex"));
        let req = AgentRequest {
            prompt: "fix tests".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            ..Default::default()
        };

        let cmd = agent.build_exec_command(&req);
        let args = args_of(&cmd);

        assert!(args.contains(&"exec".to_string()));
        assert!(args.contains(&"--full-auto".to_string()));
        assert!(!args.iter().any(|a| a == "-a" || a == "read-only"));
    }

    #[test]
    fn codex_exec_adds_model_when_requested() {
        let agent = CodexAgent::new(PathBuf::from("codex"));
        let req = AgentRequest {
            prompt: "analyze".to_string(),
            project_root: PathBuf::from("."),
            model: Some("gpt-5.1-codex".to_string()),
            ..Default::default()
        };

        let cmd = agent.build_exec_command(&req);
        let args = args_of(&cmd);

        let model_pos = args
            .iter()
            .position(|a| a == "-m")
            .expect("model flag should be present");
        assert_eq!(
            args.get(model_pos + 1).map(String::as_str),
            Some("gpt-5.1-codex")
        );
    }
}
