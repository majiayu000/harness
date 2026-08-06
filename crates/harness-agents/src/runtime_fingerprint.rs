use harness_core::config::agents::AgentsConfig;
use harness_core::stack::fingerprint::{
    digest_canonical_serializable, runner_observed_agent_runtime_component,
    AgentStackFingerprintError,
};
use harness_core::stack::{AgentStackComponent, Sha256Digest};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::process::Command;

pub const CODEX_EXEC_RUNTIME_KIND: &str = "codex_exec";
pub const CODEX_JSONRPC_RUNTIME_KIND: &str = "codex_jsonrpc";
pub const CLAUDE_CODE_RUNTIME_KIND: &str = "claude_code";
pub const RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION: &str =
    "runtime-executable-fingerprint/v0.1";

#[derive(Debug, Error)]
pub enum RuntimeFingerprintError {
    #[error(transparent)]
    Stack(#[from] AgentStackFingerprintError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredRuntimeExecutable {
    runtime_kind: String,
    executable: PathBuf,
    version_args: Vec<String>,
    declared_env_keys: Vec<String>,
}

impl ConfiguredRuntimeExecutable {
    pub fn new(runtime_kind: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self {
            runtime_kind: runtime_kind.into(),
            executable: executable.into(),
            version_args: vec!["--version".to_string()],
            declared_env_keys: Vec::new(),
        }
    }

    pub fn codex_exec(executable: impl Into<PathBuf>) -> Self {
        Self::new(CODEX_EXEC_RUNTIME_KIND, executable)
    }

    pub fn codex_jsonrpc(executable: impl Into<PathBuf>) -> Self {
        Self::new(CODEX_JSONRPC_RUNTIME_KIND, executable)
    }

    pub fn claude_code(executable: impl Into<PathBuf>) -> Self {
        Self::new(CLAUDE_CODE_RUNTIME_KIND, executable)
    }

    pub fn with_version_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.version_args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_declared_env_keys(
        mut self,
        keys: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.declared_env_keys = canonical_env_keys(keys);
        self
    }

    pub fn runtime_kind(&self) -> &str {
        &self.runtime_kind
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn version_args(&self) -> &[String] {
        &self.version_args
    }

    pub fn declared_env_keys(&self) -> &[String] {
        &self.declared_env_keys
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeFingerprintOptions {
    working_dir: Option<PathBuf>,
    environment: BTreeMap<String, String>,
    timeout: Duration,
    max_executable_bytes: u64,
    max_output_bytes: usize,
}

impl RuntimeFingerprintOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_working_dir(mut self, working_dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(working_dir.into());
        self
    }

    pub fn with_environment(
        mut self,
        environment: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.environment = environment
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_executable_bytes(mut self, max_executable_bytes: u64) -> Self {
        self.max_executable_bytes = max_executable_bytes;
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }
}

impl Default for RuntimeFingerprintOptions {
    fn default() -> Self {
        Self {
            working_dir: None,
            environment: BTreeMap::new(),
            timeout: Duration::from_secs(5),
            max_executable_bytes: 64 * 1024 * 1024,
            max_output_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentRuntimeFingerprint {
    component: AgentStackComponent,
    runtime_kind: String,
    executable: RuntimeExecutableIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<RuntimeVersionFacts>,
    environment: Vec<RuntimeEnvironmentFact>,
    failures: Vec<RuntimeProbeFailure>,
}

impl AgentRuntimeFingerprint {
    pub fn component(&self) -> &AgentStackComponent {
        &self.component
    }

    pub fn runtime_kind(&self) -> &str {
        &self.runtime_kind
    }

    pub fn executable(&self) -> &RuntimeExecutableIdentity {
        &self.executable
    }

    pub fn version(&self) -> Option<&RuntimeVersionFacts> {
        self.version.as_ref()
    }

    pub fn environment(&self) -> &[RuntimeEnvironmentFact] {
        &self.environment
    }

    pub fn failures(&self) -> &[RuntimeProbeFailure] {
        &self.failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeExecutableIdentity {
    configured_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unix_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executable_sha256: Option<Sha256Digest>,
}

impl RuntimeExecutableIdentity {
    pub fn configured_path(&self) -> &str {
        &self.configured_path
    }

    pub fn canonical_path(&self) -> Option<&str> {
        self.canonical_path.as_deref()
    }

    pub fn executable_sha256(&self) -> Option<&Sha256Digest> {
        self.executable_sha256.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeVersionFacts {
    normalized: String,
    output_sha256: Sha256Digest,
}

impl RuntimeVersionFacts {
    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    pub fn output_sha256(&self) -> &Sha256Digest {
        &self.output_sha256
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeEnvironmentFact {
    key: String,
    #[serde(flatten)]
    value: RuntimeEnvironmentValue,
}

impl RuntimeEnvironmentFact {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn value(&self) -> &RuntimeEnvironmentValue {
        &self.value
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeEnvironmentValue {
    Unset,
    SetDigest { value_sha256: Sha256Digest },
    Redacted { reason: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeProbeFailure {
    phase: &'static str,
    kind: &'static str,
    message: String,
}

impl RuntimeProbeFailure {
    fn new(phase: &'static str, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            phase,
            kind,
            message: message.into(),
        }
    }

    pub fn phase(&self) -> &str {
        self.phase
    }

    pub fn kind(&self) -> &str {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Serialize)]
struct RuntimeFingerprintPayload<'a> {
    schema_version: &'static str,
    runtime_kind: &'a str,
    executable: &'a RuntimeExecutableIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a RuntimeVersionFacts>,
    environment: &'a [RuntimeEnvironmentFact],
    failures: &'a [RuntimeProbeFailure],
}

pub async fn fingerprint_configured_runtime_executable(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
) -> Result<AgentRuntimeFingerprint, RuntimeFingerprintError> {
    let environment = environment_facts(&executable.declared_env_keys, &options.environment);
    let mut identity = RuntimeExecutableIdentity {
        configured_path: executable.executable.display().to_string(),
        canonical_path: None,
        file_name: executable
            .executable
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_owned),
        file_size_bytes: None,
        unix_mode: None,
        executable_sha256: None,
    };
    let mut failures = Vec::new();
    let mut version = None;

    if configured_path_is_unqualified(&executable.executable) {
        failures.push(RuntimeProbeFailure::new(
            "path_resolution",
            "unqualified_path_not_probed",
            "configured executable path has no directory component; PATH search was not used",
        ));
    } else {
        let metadata_path = metadata_path(&executable.executable, options.working_dir.as_deref());
        inspect_executable_identity(
            &metadata_path,
            &mut identity,
            &mut failures,
            options.max_executable_bytes,
        );
        if should_run_version_probe(&failures) {
            version = probe_runtime_version(executable, options, &mut failures).await;
        }
    }

    let payload = RuntimeFingerprintPayload {
        schema_version: RUNTIME_EXECUTABLE_FINGERPRINT_PAYLOAD_VERSION,
        runtime_kind: executable.runtime_kind.as_str(),
        executable: &identity,
        version: version.as_ref(),
        environment: &environment,
        failures: &failures,
    };
    let digest = digest_canonical_serializable(&payload)?;
    let component =
        runner_observed_agent_runtime_component(executable.runtime_kind.as_str(), digest)?;

    Ok(AgentRuntimeFingerprint {
        component,
        runtime_kind: executable.runtime_kind.clone(),
        executable: identity,
        version,
        environment,
        failures,
    })
}

pub fn configured_runtime_executables_from_agents_config(
    config: &AgentsConfig,
) -> Vec<ConfiguredRuntimeExecutable> {
    let codex_env_keys = config.codex.cloud.setup_secret_env.clone();
    vec![
        ConfiguredRuntimeExecutable::codex_exec(config.codex.cli_path.clone())
            .with_declared_env_keys(codex_env_keys.clone()),
        ConfiguredRuntimeExecutable::codex_jsonrpc(config.codex.cli_path.clone())
            .with_declared_env_keys(codex_env_keys),
        ConfiguredRuntimeExecutable::claude_code(config.claude.cli_path.clone()),
    ]
}

fn canonical_env_keys(keys: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    keys.into_iter()
        .map(Into::into)
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn environment_facts(
    declared_keys: &[String],
    environment: &BTreeMap<String, String>,
) -> Vec<RuntimeEnvironmentFact> {
    declared_keys
        .iter()
        .map(|key| {
            let value = match environment.get(key) {
                None => RuntimeEnvironmentValue::Unset,
                Some(_) if env_key_is_sensitive(key) => RuntimeEnvironmentValue::Redacted {
                    reason: "sensitive_env_key",
                },
                Some(value) => RuntimeEnvironmentValue::SetDigest {
                    value_sha256: Sha256Digest::from_bytes(value.as_bytes()),
                },
            };
            RuntimeEnvironmentFact {
                key: key.clone(),
                value,
            }
        })
        .collect()
}

fn env_key_is_sensitive(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    const MARKERS: [&str; 9] = [
        "API_KEY",
        "AUTH",
        "COOKIE",
        "CREDENTIAL",
        "DATABASE_URL",
        "PASSWORD",
        "SECRET",
        "TOKEN",
        "PRIVATE_KEY",
    ];
    MARKERS.iter().any(|marker| upper.contains(marker))
}

fn configured_path_is_unqualified(path: &Path) -> bool {
    !path.is_absolute() && path.components().count() == 1
}

fn metadata_path(configured: &Path, working_dir: Option<&Path>) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        working_dir
            .map(|root| root.join(configured))
            .unwrap_or_else(|| configured.to_path_buf())
    }
}

fn inspect_executable_identity(
    path: &Path,
    identity: &mut RuntimeExecutableIdentity,
    failures: &mut Vec<RuntimeProbeFailure>,
    max_executable_bytes: u64,
) {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            failures.push(RuntimeProbeFailure::new(
                "metadata",
                "metadata_unavailable",
                format!("failed to read configured executable metadata: {error}"),
            ));
            return;
        }
    };
    identity.canonical_path = std::fs::canonicalize(path)
        .ok()
        .map(|path| path.display().to_string());
    identity.file_size_bytes = Some(metadata.len());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        identity.unix_mode = Some(format!("{:04o}", mode & 0o7777));
        if mode & 0o111 == 0 {
            failures.push(RuntimeProbeFailure::new(
                "metadata",
                "not_executable",
                "configured executable path is not executable",
            ));
            return;
        }
    }
    if !metadata.is_file() {
        failures.push(RuntimeProbeFailure::new(
            "metadata",
            "not_regular_file",
            "configured executable path is not a regular file",
        ));
        return;
    }
    if metadata.len() > max_executable_bytes {
        failures.push(RuntimeProbeFailure::new(
            "metadata",
            "executable_too_large",
            format!(
                "configured executable is {} bytes, above the {} byte fingerprint limit",
                metadata.len(),
                max_executable_bytes
            ),
        ));
        return;
    }
    match std::fs::read(path) {
        Ok(bytes) => identity.executable_sha256 = Some(Sha256Digest::from_bytes(&bytes)),
        Err(error) => failures.push(RuntimeProbeFailure::new(
            "metadata",
            "executable_read_failed",
            format!("failed to read configured executable bytes: {error}"),
        )),
    }
}

fn should_run_version_probe(failures: &[RuntimeProbeFailure]) -> bool {
    !failures.iter().any(|failure| {
        matches!(
            failure.kind,
            "metadata_unavailable" | "not_executable" | "not_regular_file"
        )
    })
}

async fn probe_runtime_version(
    executable: &ConfiguredRuntimeExecutable,
    options: &RuntimeFingerprintOptions,
    failures: &mut Vec<RuntimeProbeFailure>,
) -> Option<RuntimeVersionFacts> {
    let mut command = Command::new(&executable.executable);
    command
        .args(&executable.version_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    if let Some(working_dir) = &options.working_dir {
        command.current_dir(working_dir);
    }
    for key in &executable.declared_env_keys {
        if let Some(value) = options
            .environment
            .get(key)
            .filter(|_| !env_key_is_sensitive(key))
        {
            command.env(key, value);
        }
    }

    let output = match tokio::time::timeout(options.timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            failures.push(RuntimeProbeFailure::new(
                "version_probe",
                "spawn_failed",
                format!("failed to spawn configured executable: {error}"),
            ));
            return None;
        }
        Err(_) => {
            failures.push(RuntimeProbeFailure::new(
                "version_probe",
                "timeout",
                format!("version probe exceeded {} ms", options.timeout.as_millis()),
            ));
            return None;
        }
    };

    if !output.status.success() {
        failures.push(RuntimeProbeFailure::new(
            "version_probe",
            "nonzero_exit",
            format!(
                "version probe exited with status {}",
                output
                    .status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ),
        ));
    }
    let mut bytes = if output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
        output.stdout
    } else {
        output.stderr
    };
    if bytes.len() > options.max_output_bytes {
        bytes.truncate(options.max_output_bytes);
        failures.push(RuntimeProbeFailure::new(
            "version_probe",
            "output_truncated",
            format!(
                "version output exceeded {} bytes and was truncated before normalization",
                options.max_output_bytes
            ),
        ));
    }
    let raw = String::from_utf8_lossy(&bytes);
    normalize_version_output(&raw).map(|normalized| RuntimeVersionFacts {
        normalized,
        output_sha256: Sha256Digest::from_bytes(&bytes),
    })
}

pub fn normalize_version_output(output: &str) -> Option<String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    line.split_whitespace()
        .find_map(normalize_version_token)
        .or_else(|| Some(line.split_whitespace().collect::<Vec<_>>().join(" ")))
}

fn normalize_version_token(token: &str) -> Option<String> {
    let trimmed = token
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != '-' && ch != '+')
        .trim_start_matches('v')
        .trim_start_matches('V');
    let has_digit = trimmed.bytes().any(|byte| byte.is_ascii_digit());
    let has_dot = trimmed.contains('.');
    (has_digit && has_dot).then(|| trimmed.to_ascii_lowercase())
}

#[cfg(test)]
mod runtime_fingerprint_tests {
    use super::*;
    use harness_core::config::agents::AgentsConfig;
    use harness_core::stack::fingerprint::{McpInputSchema, McpToolFingerprint};
    use harness_core::stack::AgentStackComponentKind;
    use serde_json::json;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn executable_script(dir: &TempDir, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.path().join("agent-runtime");
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn runtime_fingerprint_maps_agents_config_to_configured_runtime_executables() {
        let mut config = AgentsConfig::default();
        config.codex.cli_path = PathBuf::from("/opt/bin/codex");
        config.claude.cli_path = PathBuf::from("/opt/bin/claude");
        config.codex.cloud.setup_secret_env = vec!["NPM_TOKEN".to_string()];

        let executables = configured_runtime_executables_from_agents_config(&config);

        assert_eq!(executables.len(), 3);
        assert_eq!(executables[0].runtime_kind(), CODEX_EXEC_RUNTIME_KIND);
        assert_eq!(executables[1].runtime_kind(), CODEX_JSONRPC_RUNTIME_KIND);
        assert_eq!(executables[2].runtime_kind(), CLAUDE_CODE_RUNTIME_KIND);
        assert_eq!(executables[0].executable(), Path::new("/opt/bin/codex"));
        assert_eq!(executables[2].executable(), Path::new("/opt/bin/claude"));
        assert_eq!(executables[0].declared_env_keys(), &["NPM_TOKEN"]);
        assert!(executables[2].declared_env_keys().is_empty());
    }

    #[test]
    fn runtime_fingerprint_mcp_tool_digest_survives_reordered_schema() {
        let left = McpInputSchema::from_serializable(&json!({
            "type": "object",
            "required": ["prompt", "thread_id"],
            "properties": {
                "prompt": { "type": "string" },
                "thread_id": { "type": "string" }
            }
        }))
        .unwrap();
        let right = McpInputSchema::from_serializable(&json!({
            "properties": {
                "thread_id": { "type": "string" },
                "prompt": { "type": "string" }
            },
            "required": ["thread_id", "prompt"],
            "type": "object"
        }))
        .unwrap();

        let first =
            McpToolFingerprint::new("harness", "harness-reply", Some("Continue session"), left)
                .unwrap();
        let second =
            McpToolFingerprint::new("harness", "harness-reply", Some("Continue session"), right)
                .unwrap();

        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.component().kind(), AgentStackComponentKind::McpTool);
        first.component().validate().unwrap();
    }

    #[tokio::test]
    async fn runtime_fingerprint_refuses_unqualified_path_without_path_search() {
        let executable = ConfiguredRuntimeExecutable::codex_exec("codex");
        let fingerprint = fingerprint_configured_runtime_executable(
            &executable,
            &RuntimeFingerprintOptions::default(),
        )
        .await
        .unwrap();

        assert!(fingerprint
            .failures()
            .iter()
            .any(|failure| failure.kind() == "unqualified_path_not_probed"));
        assert_eq!(
            fingerprint.component().kind(),
            AgentStackComponentKind::AgentRuntime
        );
        fingerprint.component().validate().unwrap();
    }

    #[tokio::test]
    async fn runtime_fingerprint_records_unavailable_executable() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing-runtime");
        let executable = ConfiguredRuntimeExecutable::claude_code(missing);
        let fingerprint = fingerprint_configured_runtime_executable(
            &executable,
            &RuntimeFingerprintOptions::default(),
        )
        .await
        .unwrap();

        assert!(fingerprint
            .failures()
            .iter()
            .any(|failure| failure.kind() == "metadata_unavailable"));
        assert!(fingerprint.version().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runtime_fingerprint_normalizes_version_and_hashes_executable() {
        let dir = TempDir::new().unwrap();
        let path = executable_script(&dir, "#!/bin/sh\nprintf 'codex-cli version v1.2.3\\n'\n");
        let executable = ConfiguredRuntimeExecutable::codex_jsonrpc(path);
        let fingerprint = fingerprint_configured_runtime_executable(
            &executable,
            &RuntimeFingerprintOptions::default(),
        )
        .await
        .unwrap();

        assert!(fingerprint.failures().is_empty());
        assert_eq!(fingerprint.version().unwrap().normalized(), "1.2.3");
        assert!(fingerprint.executable().executable_sha256().is_some());
        assert!(fingerprint.component().integrity().is_some());
    }

    #[tokio::test]
    async fn runtime_fingerprint_redacts_only_declared_environment() {
        let executable = ConfiguredRuntimeExecutable::codex_exec("codex").with_declared_env_keys([
            "CODEX_HOME",
            "ANTHROPIC_API_KEY",
            "MISSING_ENV",
        ]);
        let options = RuntimeFingerprintOptions::default().with_environment([
            ("CODEX_HOME", "/tmp/codex-home"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("UNDECLARED_TOKEN", "do-not-include"),
        ]);
        let fingerprint = fingerprint_configured_runtime_executable(&executable, &options)
            .await
            .unwrap();
        let encoded = serde_json::to_string(&fingerprint).unwrap();

        assert!(encoded.contains("CODEX_HOME"));
        assert!(encoded.contains("ANTHROPIC_API_KEY"));
        assert!(encoded.contains("MISSING_ENV"));
        assert!(!encoded.contains("UNDECLARED_TOKEN"));
        assert!(!encoded.contains("sk-secret"));
        assert!(!encoded.contains("/tmp/codex-home"));
        assert!(fingerprint
            .environment()
            .iter()
            .any(|fact| matches!(fact.value(), RuntimeEnvironmentValue::Redacted { .. })));
        assert!(fingerprint
            .environment()
            .iter()
            .any(|fact| matches!(fact.value(), RuntimeEnvironmentValue::SetDigest { .. })));
        assert!(fingerprint
            .environment()
            .iter()
            .any(|fact| matches!(fact.value(), RuntimeEnvironmentValue::Unset)));
    }

    #[test]
    fn runtime_fingerprint_version_normalization_extracts_semver_token() {
        assert_eq!(
            normalize_version_output("Claude Code v2.1.70 (abcdef)\n").as_deref(),
            Some("2.1.70")
        );
    }
}
