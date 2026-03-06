use crate::streaming::send_stream_item;
use async_trait::async_trait;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, CodexApprovalPolicy, Item, StreamItem,
    TokenUsage,
};
use std::ffi::OsString;
use std::path::PathBuf;
use tokio::process::Command;

pub struct CodexAgent {
    pub cli_path: PathBuf,
    pub approval_policy: CodexApprovalPolicy,
}

impl CodexAgent {
    pub fn new(cli_path: PathBuf, approval_policy: CodexApprovalPolicy) -> Self {
        Self {
            cli_path,
            approval_policy,
        }
    }

    fn command_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["exec".into(), "--skip-git-repo-check".into()];

        match self.approval_policy {
            CodexApprovalPolicy::Suggest => {
                args.push("--sandbox".into());
                args.push("read-only".into());
            }
            CodexApprovalPolicy::AutoEdit => {
                args.push("--full-auto".into());
            }
            CodexApprovalPolicy::FullAuto => {
                args.push("--dangerously-bypass-approvals-and-sandbox".into());
            }
        }

        args.push("-C".into());
        args.push(req.project_root.as_os_str().to_os_string());
        args.push(req.prompt.clone().into());
        args
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
        cmd.args(self.command_args(&req));

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
    use harness_core::CodexApprovalPolicy;

    #[tokio::test]
    async fn execute_stream_returns_error_when_channel_closed() {
        let agent = CodexAgent::new(PathBuf::from("/usr/bin/true"), CodexApprovalPolicy::Suggest);
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

    #[test]
    fn suggest_policy_uses_read_only_sandbox() {
        let agent = CodexAgent::new(PathBuf::from("codex"), CodexApprovalPolicy::Suggest);
        let request = AgentRequest::default();
        let args: Vec<String> = agent
            .command_args(&request)
            .into_iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();

        assert!(args.windows(2).any(|window| {
            matches!(
                window,
                [flag, value] if flag == "--sandbox" && value == "read-only"
            )
        }));
        assert!(!args.iter().any(|arg| arg == "--full-auto"));
        assert!(!args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn auto_edit_policy_uses_full_auto_mode() {
        let agent = CodexAgent::new(PathBuf::from("codex"), CodexApprovalPolicy::AutoEdit);
        let request = AgentRequest::default();
        let args: Vec<String> = agent
            .command_args(&request)
            .into_iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();

        assert!(args.iter().any(|arg| arg == "--full-auto"));
        assert!(!args.iter().any(|arg| arg == "--sandbox"));
    }

    #[test]
    fn full_auto_policy_bypasses_approvals_and_sandbox() {
        let agent = CodexAgent::new(PathBuf::from("codex"), CodexApprovalPolicy::FullAuto);
        let request = AgentRequest::default();
        let args: Vec<String> = agent
            .command_args(&request)
            .into_iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();

        assert!(args
            .iter()
            .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
        assert!(!args.iter().any(|arg| arg == "--sandbox"));
        assert!(!args.iter().any(|arg| arg == "--full-auto"));
    }
}
