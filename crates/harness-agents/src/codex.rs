use crate::streaming::send_stream_item;
use async_trait::async_trait;
use harness_core::{
    AgentRequest, AgentResponse, Capability, CodeAgent, CodexAgentConfig, CodexCloudConfig, Item,
    StreamItem, TokenUsage,
};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::process::Command;

pub struct CodexAgent {
    pub cli_path: PathBuf,
    pub cloud: CodexCloudConfig,
}

impl CodexAgent {
    pub fn new(cli_path: PathBuf) -> Self {
        Self::with_cloud(cli_path, CodexCloudConfig::default())
    }

    pub fn with_cloud(cli_path: PathBuf, cloud: CodexCloudConfig) -> Self {
        Self { cli_path, cloud }
    }

    pub fn from_config(config: CodexAgentConfig) -> Self {
        Self::with_cloud(config.cli_path, config.cloud)
    }

    fn sandbox_mode(&self) -> &'static str {
        if self.cloud.enabled {
            "workspace-write"
        } else {
            "read-only"
        }
    }

    fn setup_cache_ttl(&self) -> Duration {
        Duration::from_secs(self.cloud.cache_ttl_hours.saturating_mul(3600))
    }

    fn setup_cache_key(&self, project_root: &Path) -> u64 {
        let mut hasher = DefaultHasher::new();
        project_root.hash(&mut hasher);
        self.cloud.setup_commands.hash(&mut hasher);
        self.cloud.setup_secret_env.hash(&mut hasher);
        self.cloud.cache_ttl_hours.hash(&mut hasher);
        hasher.finish()
    }

    fn setup_cache_stamp_path(&self, project_root: &Path) -> PathBuf {
        project_root
            .join(".harness")
            .join("cloud-setup-cache")
            .join(format!("{:016x}.stamp", self.setup_cache_key(project_root)))
    }

    fn setup_cache_is_fresh(&self, project_root: &Path) -> harness_core::Result<bool> {
        if self.cloud.cache_ttl_hours == 0 {
            return Ok(false);
        }

        let stamp = self.setup_cache_stamp_path(project_root);
        let metadata = match fs::metadata(&stamp) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => {
                return Err(harness_core::HarnessError::AgentExecution(format!(
                    "failed to read cloud setup cache metadata `{}`: {err}",
                    stamp.display()
                )));
            }
        };

        let modified = metadata.modified().map_err(|err| {
            harness_core::HarnessError::AgentExecution(format!(
                "failed to read cloud setup cache mtime `{}`: {err}",
                stamp.display()
            ))
        })?;

        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);

        Ok(age <= self.setup_cache_ttl())
    }

    fn write_setup_cache_stamp(&self, project_root: &Path) -> harness_core::Result<()> {
        if self.cloud.cache_ttl_hours == 0 {
            return Ok(());
        }

        let stamp = self.setup_cache_stamp_path(project_root);
        let Some(parent) = stamp.parent() else {
            return Err(harness_core::HarnessError::AgentExecution(format!(
                "invalid cloud setup cache path `{}`",
                stamp.display()
            )));
        };

        fs::create_dir_all(parent).map_err(|err| {
            harness_core::HarnessError::AgentExecution(format!(
                "failed to create cloud setup cache dir `{}`: {err}",
                parent.display()
            ))
        })?;

        fs::write(&stamp, b"ok\n").map_err(|err| {
            harness_core::HarnessError::AgentExecution(format!(
                "failed to write cloud setup cache stamp `{}`: {err}",
                stamp.display()
            ))
        })?;

        Ok(())
    }

    fn apply_setup_environment(&self, cmd: &mut Command) {
        cmd.env_clear();

        for key in [
            "PATH", "HOME", "USER", "SHELL", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "LC_CTYPE",
        ] {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }

        for key in &self.cloud.setup_secret_env {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
    }

    async fn run_setup_phase(&self, project_root: &Path) -> harness_core::Result<()> {
        if !self.cloud.enabled || self.cloud.setup_commands.is_empty() {
            return Ok(());
        }

        if self.setup_cache_is_fresh(project_root)? {
            return Ok(());
        }

        for setup_command in &self.cloud.setup_commands {
            if setup_command.trim().is_empty() {
                continue;
            }

            let mut cmd = Command::new("sh");
            cmd.arg("-lc").arg(setup_command).current_dir(project_root);
            self.apply_setup_environment(&mut cmd);

            let output = cmd.output().await.map_err(|err| {
                harness_core::HarnessError::AgentExecution(format!(
                    "failed to run cloud setup command `{setup_command}`: {err}"
                ))
            })?;

            if !output.status.success() {
                let detail = command_output_summary(&output);
                return Err(harness_core::HarnessError::AgentExecution(format!(
                    "cloud setup command `{setup_command}` failed with {}: {detail}",
                    output.status
                )));
            }
        }

        self.write_setup_cache_stamp(project_root)?;
        Ok(())
    }

    fn agent_phase_args(&self, req: &AgentRequest) -> Vec<OsString> {
        let mut args = vec![
            OsString::from("exec"),
            OsString::from("--skip-git-repo-check"),
            OsString::from("--sandbox"),
            OsString::from(self.sandbox_mode()),
        ];

        if self.cloud.enabled {
            args.push(OsString::from("--ephemeral"));
        }

        args.push(OsString::from("-C"));
        args.push(req.project_root.as_os_str().to_os_string());
        args.push(OsString::from(req.prompt.clone()));
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
        self.run_setup_phase(&req.project_root).await?;

        let mut cmd = Command::new(&self.cli_path);
        cmd.args(self.agent_phase_args(&req));

        if self.cloud.enabled {
            for key in &self.cloud.setup_secret_env {
                cmd.env_remove(key);
            }
        }

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

fn command_output_summary(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }

    "no output".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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

    #[test]
    fn local_mode_uses_read_only_sandbox_without_ephemeral() {
        let agent = CodexAgent::new(PathBuf::from("codex"));
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .agent_phase_args(&request)
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(!args.iter().any(|a| a == "--ephemeral"));
    }

    #[test]
    fn cloud_mode_uses_workspace_write_and_ephemeral() {
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: Vec::new(),
            setup_secret_env: Vec::new(),
        };
        let agent = CodexAgent::with_cloud(PathBuf::from("codex"), cloud);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: PathBuf::from("/tmp/project"),
            ..Default::default()
        };

        let args: Vec<String> = agent
            .agent_phase_args(&request)
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect();

        assert!(args
            .windows(2)
            .any(|window| window == ["--sandbox", "workspace-write"]));
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
    }

    #[tokio::test]
    async fn cloud_setup_phase_uses_cache_within_ttl() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let marker = dir.path().join("setup-runs.log");
        let setup = format!("echo run >> \"{}\"", marker.display());
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec![setup],
            setup_secret_env: Vec::new(),
        };

        let agent = CodexAgent::with_cloud(PathBuf::from("/usr/bin/true"), cloud);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: dir.path().to_path_buf(),
            ..Default::default()
        };

        agent.execute(request.clone()).await?;
        agent.execute(request).await?;

        let log = fs::read_to_string(marker)?;
        assert_eq!(log.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn cloud_setup_cache_invalidation_uses_config_hash() {
        let project_root = Path::new("/tmp/project");
        let first = CodexAgent::with_cloud(
            PathBuf::from("codex"),
            CodexCloudConfig {
                enabled: true,
                cache_ttl_hours: 12,
                setup_commands: vec!["npm ci".to_string()],
                setup_secret_env: vec!["NPM_TOKEN".to_string()],
            },
        );
        let second = CodexAgent::with_cloud(
            PathBuf::from("codex"),
            CodexCloudConfig {
                enabled: true,
                cache_ttl_hours: 12,
                setup_commands: vec!["cargo fetch".to_string()],
                setup_secret_env: vec!["NPM_TOKEN".to_string()],
            },
        );

        assert_ne!(
            first.setup_cache_key(project_root),
            second.setup_cache_key(project_root)
        );
    }

    #[tokio::test]
    async fn setup_secret_is_available_in_setup_but_removed_for_agent_phase() -> anyhow::Result<()>
    {
        let dir = tempdir()?;
        let setup_capture = dir.path().join("setup-secret.txt");
        let agent_capture = dir.path().join("agent-env.txt");
        let cli_script = dir.path().join("capture-agent-env.sh");

        fs::write(
            &cli_script,
            format!("#!/bin/sh\nenv > \"{}\"\nexit 0\n", agent_capture.display()),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut perms = fs::metadata(&cli_script)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&cli_script, perms)?;
        }

        let secret_name = "HARNESS_CODEX_SETUP_SECRET";
        let secret_value = "top-secret-token";
        std::env::set_var(secret_name, secret_value);

        let setup = format!(
            "printf '%s' \"${secret_name}\" > \"{}\"",
            setup_capture.display()
        );

        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec![setup],
            setup_secret_env: vec![secret_name.to_string()],
        };

        let agent = CodexAgent::with_cloud(cli_script, cloud);
        let request = AgentRequest {
            prompt: "ping".to_string(),
            project_root: dir.path().to_path_buf(),
            ..Default::default()
        };

        let result = agent.execute(request).await;
        std::env::remove_var(secret_name);
        result?;

        let setup_secret = fs::read_to_string(setup_capture)?;
        assert_eq!(setup_secret, secret_value);

        let agent_env = fs::read_to_string(agent_capture)?;
        assert!(
            !agent_env.contains(&format!("{secret_name}={secret_value}")),
            "setup secret leaked into agent phase environment"
        );
        Ok(())
    }
}
