use super::super::model::RuntimeKind;
use harness_core::config::isolation::IsolationTier;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::{error::Error, fmt};

pub const DEFAULT_CASE_TIMEOUT_SECS: u64 = 3_600;
pub const DEFAULT_EVAL_ISOLATION_RUNTIME_PROFILE: &str = "eval-isolated-runtime-host";
pub const DEFAULT_EVAL_ISOLATION_SANDBOX: &str = "workspace-write";
pub const DEFAULT_EVAL_ISOLATION_BACKEND: &str = "container_runtime_host";
pub const DEFAULT_EVAL_ISOLATION_IMAGE: &str = "harness-eval-runner:local";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EvalBenchmarkManifest {
    pub suite: String,
    pub cases: Vec<EvalBenchmarkCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EvalBenchmarkCase {
    pub case_id: String,
    pub repo: String,
    pub issue: u64,
    pub base_commit: String,
    pub verify_commands: Vec<String>,
    pub timeout_secs: u64,
    pub isolation: EvalIsolationProfile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalIsolationProfile {
    #[serde(default = "default_eval_isolation_tier")]
    pub tier: IsolationTier,
    #[serde(default = "default_eval_isolation_runtime_kind")]
    pub runtime_kind: RuntimeKind,
    #[serde(default = "default_eval_isolation_runtime_profile")]
    pub runtime_profile: String,
    #[serde(default = "default_eval_isolation_sandbox")]
    pub sandbox: String,
    #[serde(default = "default_eval_isolation_backend")]
    pub backend: String,
    #[serde(default = "default_eval_isolation_image")]
    pub image: String,
    #[serde(default)]
    pub lifecycle: EvalIsolationLifecycle,
    #[serde(default = "default_cleanup_required")]
    pub cleanup_required: bool,
}

impl Default for EvalIsolationProfile {
    fn default() -> Self {
        Self {
            tier: default_eval_isolation_tier(),
            runtime_kind: default_eval_isolation_runtime_kind(),
            runtime_profile: default_eval_isolation_runtime_profile(),
            sandbox: default_eval_isolation_sandbox(),
            backend: default_eval_isolation_backend(),
            image: default_eval_isolation_image(),
            lifecycle: EvalIsolationLifecycle::default(),
            cleanup_required: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalIsolationLifecycle {
    #[default]
    Ephemeral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestError {
    message: String,
}

impl ManifestError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ManifestError {}

#[derive(Deserialize)]
struct RawManifest {
    suite: String,
    #[serde(default)]
    default_timeout_secs: Option<u64>,
    #[serde(default)]
    isolation: EvalIsolationProfile,
    cases: Vec<RawCase>,
}

#[derive(Deserialize)]
struct RawCase {
    #[serde(default)]
    case_id: Option<String>,
    repo: String,
    issue: u64,
    base_commit: String,
    verify_commands: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    isolation: Option<EvalIsolationProfile>,
}

pub fn parse_benchmark_manifest_str(input: &str) -> Result<EvalBenchmarkManifest, ManifestError> {
    let raw: RawManifest =
        toml::from_str(input).map_err(|err| ManifestError::new(format!("invalid TOML: {err}")))?;
    normalize_manifest(raw)
}

fn normalize_manifest(raw: RawManifest) -> Result<EvalBenchmarkManifest, ManifestError> {
    let suite = non_empty(raw.suite, "suite")?;
    if raw.cases.is_empty() {
        return Err(ManifestError::new(
            "manifest must contain at least one case",
        ));
    }

    let default_timeout_secs = raw
        .default_timeout_secs
        .unwrap_or(DEFAULT_CASE_TIMEOUT_SECS);
    validate_timeout(default_timeout_secs, "default_timeout_secs")?;
    let default_isolation = normalize_isolation_profile(raw.isolation, "manifest eval isolation")?;

    let mut seen_case_ids = BTreeSet::new();
    let mut cases = Vec::with_capacity(raw.cases.len());
    for (index, case) in raw.cases.into_iter().enumerate() {
        let repo = non_empty(case.repo, "case repo")?;
        validate_repo(&repo)?;
        if case.issue == 0 {
            return Err(ManifestError::new(format!(
                "case {} issue must be greater than zero",
                index + 1
            )));
        }
        let base_commit = non_empty(case.base_commit, "base_commit")?;
        validate_base_commit(&base_commit)?;
        let verify_commands = normalize_verify_commands(case.verify_commands, index)?;
        let timeout_secs = case.timeout_secs.unwrap_or(default_timeout_secs);
        validate_timeout(timeout_secs, "timeout_secs")?;
        let isolation = match case.isolation {
            Some(profile) => {
                let context = format!("case {} eval isolation", index + 1);
                normalize_isolation_profile(profile, &context)?
            }
            None => default_isolation.clone(),
        };
        let case_id = case
            .case_id
            .map(|id| non_empty(id, "case_id"))
            .transpose()?
            .unwrap_or_else(|| format!("{repo}#{}", case.issue));
        if !seen_case_ids.insert(case_id.clone()) {
            return Err(ManifestError::new(format!(
                "duplicate benchmark case_id: {case_id}"
            )));
        }

        cases.push(EvalBenchmarkCase {
            case_id,
            repo,
            issue: case.issue,
            base_commit,
            verify_commands,
            timeout_secs,
            isolation,
        });
    }

    Ok(EvalBenchmarkManifest { suite, cases })
}

fn non_empty(value: String, field: &str) -> Result<String, ManifestError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::new(format!("{field} must not be empty")));
    }
    if trimmed.len() == value.len() {
        Ok(value)
    } else {
        Ok(trimmed.to_string())
    }
}

fn validate_repo(repo: &str) -> Result<(), ManifestError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(ManifestError::new(format!(
            "repo must use owner/name syntax: {repo}"
        )));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || repo.chars().any(char::is_whitespace)
    {
        return Err(ManifestError::new(format!(
            "repo must use owner/name syntax: {repo}"
        )));
    }
    Ok(())
}

fn validate_base_commit(base_commit: &str) -> Result<(), ManifestError> {
    let len = base_commit.len();
    if !(7..=40).contains(&len) || !base_commit.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ManifestError::new(format!(
            "base_commit must be a 7 to 40 character hex commit: {base_commit}"
        )));
    }
    Ok(())
}

fn normalize_verify_commands(
    verify_commands: Vec<String>,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    if verify_commands.is_empty() {
        return Err(ManifestError::new(format!(
            "case {} must include at least one verify command",
            case_index + 1
        )));
    }
    verify_commands
        .into_iter()
        .map(|command| non_empty(command, "verify command"))
        .collect()
}

fn validate_timeout(timeout_secs: u64, field: &str) -> Result<(), ManifestError> {
    if timeout_secs == 0 {
        return Err(ManifestError::new(format!(
            "{field} must be greater than zero"
        )));
    }
    Ok(())
}

fn normalize_isolation_profile(
    mut profile: EvalIsolationProfile,
    context: &str,
) -> Result<EvalIsolationProfile, ManifestError> {
    profile.runtime_profile = non_empty(profile.runtime_profile, "eval isolation runtime_profile")?;
    profile.sandbox = non_empty(profile.sandbox, "eval isolation sandbox")?;
    profile.backend = non_empty(profile.backend, "eval isolation backend")?;
    profile.image = non_empty(profile.image, "eval isolation image")?;

    match profile.tier {
        IsolationTier::Host => {
            return Err(ManifestError::new(format!(
                "{context} tier must be container; host is not valid for untrusted eval cases"
            )));
        }
        IsolationTier::Container => {}
        IsolationTier::Microvm => {
            return Err(ManifestError::new(format!(
                "{context} tier `microvm` is reserved but not implemented; use container"
            )));
        }
    }
    if profile.runtime_kind != RuntimeKind::RemoteHost {
        return Err(ManifestError::new(format!(
            "{context} runtime_kind must be remote_host so eval cases cannot run in the caller or server process"
        )));
    }
    if profile.sandbox != DEFAULT_EVAL_ISOLATION_SANDBOX {
        return Err(ManifestError::new(format!(
            "{context} sandbox must be {DEFAULT_EVAL_ISOLATION_SANDBOX}"
        )));
    }
    if !profile.cleanup_required {
        return Err(ManifestError::new(format!(
            "{context} cleanup_required must be true"
        )));
    }

    Ok(profile)
}

fn default_eval_isolation_tier() -> IsolationTier {
    IsolationTier::Container
}

fn default_eval_isolation_runtime_kind() -> RuntimeKind {
    RuntimeKind::RemoteHost
}

fn default_eval_isolation_runtime_profile() -> String {
    DEFAULT_EVAL_ISOLATION_RUNTIME_PROFILE.to_string()
}

fn default_eval_isolation_sandbox() -> String {
    DEFAULT_EVAL_ISOLATION_SANDBOX.to_string()
}

fn default_eval_isolation_backend() -> String {
    DEFAULT_EVAL_ISOLATION_BACKEND.to_string()
}

fn default_eval_isolation_image() -> String {
    DEFAULT_EVAL_ISOLATION_IMAGE.to_string()
}

fn default_cleanup_required() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_MANIFEST: &str = r#"
suite = "harness-core"
default_timeout_secs = 7200

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test -p harness-server lifecycle_"]

[[cases]]
case_id = "stall-timeout-control"
repo = "majiayu000/harness"
issue = 1443
base_commit = "956076f02f546058960bf10d7a00157e5f0139dd"
verify_commands = ["cargo test -p harness-server turn_lifecycle"]
timeout_secs = 1800
"#;

    #[test]
    fn eval_manifest_parses_cases_with_defaults() {
        let manifest = parse_benchmark_manifest_str(VALID_MANIFEST).expect("manifest should parse");

        assert_eq!(manifest.suite, "harness-core");
        assert_eq!(manifest.cases.len(), 2);
        assert_eq!(manifest.cases[0].case_id, "majiayu000/harness#1437");
        assert_eq!(manifest.cases[0].timeout_secs, 7200);
        assert_eq!(manifest.cases[0].isolation.tier, IsolationTier::Container);
        assert_eq!(
            manifest.cases[0].isolation.runtime_kind,
            RuntimeKind::RemoteHost
        );
        assert_eq!(
            manifest.cases[0].isolation.runtime_profile,
            DEFAULT_EVAL_ISOLATION_RUNTIME_PROFILE
        );
        assert!(manifest.cases[0].isolation.cleanup_required);
        assert_eq!(manifest.cases[1].case_id, "stall-timeout-control");
        assert_eq!(manifest.cases[1].timeout_secs, 1800);
    }

    #[test]
    fn eval_isolation_fixture_selects_remote_container_profile() {
        let manifest = parse_benchmark_manifest_str(include_str!(
            "../../../../../evals/benchmarks/eval-isolation-fixture.toml"
        ))
        .expect("fixture manifest should parse");

        let case = &manifest.cases[0];
        assert_eq!(case.isolation.tier, IsolationTier::Container);
        assert_eq!(case.isolation.runtime_kind, RuntimeKind::RemoteHost);
        assert_eq!(case.isolation.lifecycle, EvalIsolationLifecycle::Ephemeral);
        assert_eq!(case.isolation.backend, "container_runtime_host");
        assert_eq!(case.isolation.image, "harness-eval-runner:local");
        assert!(case.isolation.cleanup_required);
    }

    #[test]
    fn eval_isolation_rejects_host_tier() {
        let input = r#"
suite = "harness-core"

[isolation]
tier = "host"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("host eval tier should fail");
        assert!(err.to_string().contains("tier must be container"));
    }

    #[test]
    fn eval_isolation_rejects_local_runtime_kind() {
        let input = r#"
suite = "harness-core"

[isolation]
runtime_kind = "codex_jsonrpc"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("local eval runtime should fail");
        assert!(err.to_string().contains("runtime_kind must be remote_host"));
    }

    #[test]
    fn eval_isolation_rejects_missing_cleanup_requirement() {
        let input = r#"
suite = "harness-core"

[isolation]
cleanup_required = false

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("eval cleanup must be required");
        assert!(err.to_string().contains("cleanup_required must be true"));
    }

    #[test]
    fn eval_manifest_rejects_duplicate_case_ids() {
        let input = r#"
suite = "harness-core"

[[cases]]
case_id = "same"
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]

[[cases]]
case_id = "same"
repo = "majiayu000/harness"
issue = 1443
base_commit = "956076f0"
verify_commands = ["cargo test"]
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("duplicate id should fail");
        assert!(err.to_string().contains("duplicate benchmark case_id"));
    }

    #[test]
    fn eval_manifest_rejects_missing_verify_commands() {
        let input = r#"
suite = "harness-core"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = []
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("empty verify list should fail");
        assert!(err.to_string().contains("at least one verify command"));
    }

    #[test]
    fn eval_manifest_rejects_non_hex_base_commit() {
        let input = r#"
suite = "harness-core"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "main"
verify_commands = ["cargo test"]
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("non-hex commit should fail");
        assert!(err.to_string().contains("base_commit"));
    }
}
