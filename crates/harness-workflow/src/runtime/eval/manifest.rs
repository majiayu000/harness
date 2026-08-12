use super::super::model::RuntimeKind;
use harness_core::config::isolation::IsolationTier;
use harness_sandbox::{CappedResourceLimits, ResourceLimits};
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
    pub paths: Vec<String>,
    pub risk: Option<EvalCaseRisk>,
    pub evidence: Vec<String>,
    pub resolution_prs: Vec<u64>,
    pub resolution_commits: Vec<String>,
    pub commit_resolution: Option<EvalCommitResolution>,
    pub verdict: Option<EvalCaseVerdict>,
    pub timeout_secs: u64,
    pub resource_limits: CappedResourceLimits,
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

impl EvalBenchmarkCase {
    pub fn replay_blocker(&self) -> Option<&'static str> {
        match self.commit_resolution {
            Some(EvalCommitResolution::Pending) => Some("commit_resolution is pending"),
            Some(EvalCommitResolution::Resolved) if self.resolution_commits.is_empty() => {
                Some("resolved commit_resolution has no resolution_commits")
            }
            None if self.verdict == Some(EvalCaseVerdict::Replayable) => {
                Some("replayable verdict has no commit_resolution")
            }
            _ if self.verdict == Some(EvalCaseVerdict::Pending) => Some("verdict is pending"),
            _ => None,
        }
    }

    pub fn is_replayable(&self) -> bool {
        self.replay_blocker().is_none()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseRisk {
    Low,
    Medium,
    High,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCommitResolution {
    Pending,
    Resolved,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCaseVerdict {
    Pending,
    Replayable,
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
    #[serde(default)]
    default_resource_limits: ResourceLimits,
    #[serde(default)]
    max_resource_limits: ResourceLimits,
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
    paths: Vec<String>,
    #[serde(default)]
    risk: Option<EvalCaseRisk>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    resolution_prs: Vec<u64>,
    #[serde(default)]
    resolution_commits: Vec<String>,
    #[serde(default)]
    commit_resolution: Option<EvalCommitResolution>,
    #[serde(default)]
    verdict: Option<EvalCaseVerdict>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    isolation: Option<EvalIsolationProfile>,
    #[serde(default)]
    resource_limits: ResourceLimits,
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
    let operator_maxima =
        ResourceLimits::operator_default_maxima().overlay(raw.max_resource_limits);

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
        let paths = normalize_paths(case.paths, index)?;
        let evidence = normalize_evidence(case.evidence, index)?;
        let resolution_prs = normalize_resolution_prs(case.resolution_prs, index)?;
        let resolution_commits = normalize_resolution_commits(case.resolution_commits, index)?;
        validate_resolution_metadata(
            case.commit_resolution,
            case.verdict,
            &resolution_prs,
            &resolution_commits,
            index,
        )?;
        let timeout_secs = case.timeout_secs.unwrap_or(default_timeout_secs);
        validate_timeout(timeout_secs, "timeout_secs")?;
        let isolation = match case.isolation {
            Some(profile) => {
                let context = format!("case {} eval isolation", index + 1);
                normalize_isolation_profile(profile, &context)?
            }
            None => default_isolation.clone(),
        };
        let resource_limits = ResourceLimits::evaluation_defaults(timeout_secs)
            .overlay(raw.default_resource_limits)
            .overlay(case.resource_limits)
            .cap_by(operator_maxima)
            .map_err(|error| ManifestError::new(format!("invalid resource_limits: {error}")))?;
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
            paths,
            risk: case.risk,
            evidence,
            resolution_prs,
            resolution_commits,
            commit_resolution: case.commit_resolution,
            verdict: case.verdict,
            timeout_secs,
            resource_limits,
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
        .map(|command| {
            let command = non_empty(command, "verify command")?;
            validate_command_structure(&command)?;
            Ok(command)
        })
        .collect()
}

fn normalize_paths(paths: Vec<String>, case_index: usize) -> Result<Vec<String>, ManifestError> {
    paths
        .into_iter()
        .map(|path| {
            let path = non_empty(path, "path")?;
            validate_repo_relative_path(&path)
                .map_err(|error| ManifestError::new(format!("case {} {error}", case_index + 1)))?;
            Ok(path)
        })
        .collect()
}

fn normalize_evidence(
    evidence: Vec<String>,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    evidence
        .into_iter()
        .map(|evidence| {
            let evidence = non_empty(evidence, "evidence")?;
            validate_single_line(&evidence, "evidence")
                .map_err(|error| ManifestError::new(format!("case {} {error}", case_index + 1)))?;
            Ok(evidence)
        })
        .collect()
}

fn normalize_resolution_prs(
    resolution_prs: Vec<u64>,
    case_index: usize,
) -> Result<Vec<u64>, ManifestError> {
    if resolution_prs.contains(&0) {
        return Err(ManifestError::new(format!(
            "case {} resolution_prs must be greater than zero",
            case_index + 1
        )));
    }
    Ok(resolution_prs)
}

fn normalize_resolution_commits(
    resolution_commits: Vec<String>,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    resolution_commits
        .into_iter()
        .map(|commit| {
            let commit = non_empty(commit, "resolution_commit")?;
            validate_base_commit(&commit)
                .map_err(|error| ManifestError::new(format!("case {} {error}", case_index + 1)))?;
            Ok(commit)
        })
        .collect()
}

fn validate_resolution_metadata(
    commit_resolution: Option<EvalCommitResolution>,
    verdict: Option<EvalCaseVerdict>,
    resolution_prs: &[u64],
    resolution_commits: &[String],
    case_index: usize,
) -> Result<(), ManifestError> {
    if (!resolution_prs.is_empty() || !resolution_commits.is_empty()) && commit_resolution.is_none()
    {
        return Err(ManifestError::new(format!(
            "case {} commit_resolution is required when resolution metadata is present",
            case_index + 1
        )));
    }

    match commit_resolution {
        Some(EvalCommitResolution::Resolved) if resolution_commits.is_empty() => {
            Err(ManifestError::new(format!(
                "case {} resolved commit_resolution requires resolution_commits",
                case_index + 1
            )))
        }
        Some(EvalCommitResolution::Pending) if !resolution_commits.is_empty() => {
            Err(ManifestError::new(format!(
                "case {} pending commit_resolution must not include resolution_commits",
                case_index + 1
            )))
        }
        Some(EvalCommitResolution::Pending) if verdict == Some(EvalCaseVerdict::Replayable) => {
            Err(ManifestError::new(format!(
                "case {} pending commit_resolution cannot be replayable",
                case_index + 1
            )))
        }
        None if verdict == Some(EvalCaseVerdict::Replayable) => Err(ManifestError::new(format!(
            "case {} replayable verdict requires resolved commit_resolution",
            case_index + 1
        ))),
        _ => Ok(()),
    }
}

fn validate_command_structure(command: &str) -> Result<(), ManifestError> {
    validate_single_line(command, "verify command")?;
    let Some(program) = command.split_whitespace().next() else {
        return Err(ManifestError::new("verify command must include a program"));
    };
    if program.starts_with('-') {
        return Err(ManifestError::new(format!(
            "verify command program must not start with '-': {command}"
        )));
    }
    Ok(())
}

fn validate_repo_relative_path(path: &str) -> Result<(), ManifestError> {
    if path.starts_with('/') || path.starts_with('~') || path.contains('\\') {
        return Err(ManifestError::new(format!(
            "path must be repository-relative: {path}"
        )));
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ManifestError::new(format!(
            "path must not contain empty, current, or parent segments: {path}"
        )));
    }
    Ok(())
}

fn validate_single_line(value: &str, field: &str) -> Result<(), ManifestError> {
    if value.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0')) {
        return Err(ManifestError::new(format!("{field} must be a single line")));
    }
    Ok(())
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
paths = ["crates/harness-server/src/workflow_runtime_worker.rs"]
risk = "high"
evidence = [
    "https://github.com/majiayu000/harness/issues/1437",
    "specs/GH1437/tasks.md",
]
resolution_prs = [1502]
resolution_commits = ["0123456789abcdef"]
commit_resolution = "resolved"
verdict = "replayable"

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
        assert_eq!(manifest.cases[0].risk, Some(EvalCaseRisk::High));
        assert_eq!(
            manifest.cases[0].commit_resolution,
            Some(EvalCommitResolution::Resolved)
        );
        assert_eq!(manifest.cases[0].verdict, Some(EvalCaseVerdict::Replayable));
        assert_eq!(manifest.cases[0].resolution_prs, vec![1502]);
        assert_eq!(
            manifest.cases[0].paths,
            vec!["crates/harness-server/src/workflow_runtime_worker.rs"]
        );
        assert_eq!(
            manifest.cases[0].resource_limits.effective.wall_time_secs,
            Some(7200)
        );
        assert_eq!(manifest.cases[1].case_id, "stall-timeout-control");
        assert_eq!(manifest.cases[1].timeout_secs, 1800);
        assert_eq!(
            manifest.cases[1].resource_limits.effective.cpu_time_secs,
            Some(1800)
        );
    }

    #[test]
    fn eval_manifest_caps_resource_limits_by_operator_maxima() {
        let input = r#"
suite = "harness-core"
default_timeout_secs = 120

[max_resource_limits]
cpu_time_secs = 60
memory_bytes = 16777216
output_bytes = 2048
wall_time_secs = 90

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
resource_limits = { cpu_time_secs = 120, memory_bytes = 33554432, output_bytes = 1024, wall_time_secs = 180 }
"#;

        let manifest = parse_benchmark_manifest_str(input).expect("manifest should parse");
        let limits = &manifest.cases[0].resource_limits;

        assert_eq!(limits.effective.cpu_time_secs, Some(60));
        assert_eq!(limits.effective.memory_bytes, Some(16777216));
        assert_eq!(limits.effective.output_bytes, Some(1024));
        assert_eq!(limits.effective.wall_time_secs, Some(90));
        assert_eq!(limits.caps.len(), 3);
    }

    #[test]
    fn eval_manifest_rejects_zero_resource_limits() {
        let input = r#"
suite = "harness-core"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
resource_limits = { memory_bytes = 0 }
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("zero limit should fail");
        assert!(err.to_string().contains("resource limit `memory`"));
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

    #[test]
    fn eval_manifest_accepts_pending_resolution_without_replayable_verdict() {
        let input = r#"
suite = "harness-core"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
commit_resolution = "pending"
verdict = "pending"
"#;

        let manifest = parse_benchmark_manifest_str(input).expect("pending case should parse");
        assert_eq!(
            manifest.cases[0].commit_resolution,
            Some(EvalCommitResolution::Pending)
        );
        assert_eq!(manifest.cases[0].verdict, Some(EvalCaseVerdict::Pending));
        assert!(manifest.cases[0].resolution_commits.is_empty());
        assert_eq!(
            manifest.cases[0].replay_blocker(),
            Some("commit_resolution is pending")
        );
        assert!(!manifest.cases[0].is_replayable());
    }

    #[test]
    fn eval_manifest_rejects_replayable_pending_resolution() {
        let input = r#"
suite = "harness-core"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
commit_resolution = "pending"
verdict = "replayable"
"#;

        let err =
            parse_benchmark_manifest_str(input).expect_err("pending replayable case should fail");
        assert!(err.to_string().contains("pending commit_resolution"));
    }

    #[test]
    fn eval_manifest_rejects_invalid_paths() {
        let input = r#"
suite = "harness-core"

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test"]
paths = ["../Cargo.toml"]
"#;

        let err = parse_benchmark_manifest_str(input).expect_err("parent path should fail");
        assert!(err.to_string().contains("path must not contain"));
    }

    #[test]
    fn historical_replay_manifest_parses_selected_cases() {
        let manifest = parse_benchmark_manifest_str(include_str!(
            "../../../../../evals/benchmarks/harness-historical-replay.toml"
        ))
        .expect("historical replay manifest should parse");
        let issues = manifest
            .cases
            .iter()
            .map(|case| case.issue)
            .collect::<Vec<_>>();
        assert_eq!(
            issues,
            vec![1715, 1716, 1717, 1434, 1707, 1704, 1686, 1652, 1574, 1656]
        );

        for case in &manifest.cases {
            assert!(case.risk.is_some(), "{} missing risk", case.case_id);
            assert!(
                !case.evidence.is_empty(),
                "{} missing evidence",
                case.case_id
            );
            assert!(!case.paths.is_empty(), "{} missing paths", case.case_id);
            assert!(
                !case.resolution_prs.is_empty(),
                "{} missing resolution PRs",
                case.case_id
            );
            assert!(
                !case.resolution_commits.is_empty(),
                "{} missing resolution commits",
                case.case_id
            );
            assert_eq!(
                case.commit_resolution,
                Some(EvalCommitResolution::Resolved),
                "{} must be resolved",
                case.case_id
            );
            assert_eq!(
                case.verdict,
                Some(EvalCaseVerdict::Replayable),
                "{} must be replayable",
                case.case_id
            );
        }

        let gh1717 = manifest
            .cases
            .iter()
            .find(|case| case.issue == 1717)
            .expect("GH-1717 case exists");
        assert_eq!(gh1717.resolution_prs, vec![1723, 1724]);
        assert_eq!(gh1717.resolution_commits.len(), 2);
    }

    #[test]
    fn historical_replay_manifest_uses_non_skipping_oracles() {
        let manifest = parse_benchmark_manifest_str(include_str!(
            "../../../../../evals/benchmarks/harness-historical-replay.toml"
        ))
        .expect("historical replay manifest should parse");

        assert_command_contains(
            &manifest,
            1716,
            "test -n \"$HARNESS_DATABASE_URL\" && cargo test -p harness-server --lib task_db::queries_recovery_tests",
        );
        assert_command_contains(
            &manifest,
            1704,
            "test -n \"$HARNESS_DATABASE_URL\" && cargo test -p harness-server 'http::tests::runtime_transcript_route_tests::exact_replay_preflight_fails_terminal_on_missing_or_corrupt_transcript' -- --exact",
        );
        assert_command_contains(
            &manifest,
            1707,
            "test -n \"$HARNESS_DATABASE_URL\" && cargo test -p harness-server empty_store_recovers_ready_pr_and_stays_idempotent_after_restart",
        );
        assert_command_contains(
            &manifest,
            1434,
            "bash scripts/archive-phase1-data.sh && test -s archives/phase1-*/RESTORE.md",
        );
        assert_command_contains(&manifest, 1574, "phase1-replay-report.json");
        assert_command_contains(&manifest, 1686, "bun-version-file: .bun-version");
        assert_command_contains(
            &manifest,
            1717,
            "cargo test -p harness-server context_rpc_preview_with_supplied_items_returns_manifest --lib",
        );
        assert_command_contains(&manifest, 1717, "cargo tree -p harness-protocol");
        assert_command_contains(
            &manifest,
            1717,
            "cargo test -p harness-protocol context_preview_defaults_and_empty_collections_are_equivalent --lib",
        );
        assert_command_contains(
            &manifest,
            1717,
            "cargo test -p harness-server context_preview_conversion_is_deterministic_and_order_preserving --lib",
        );
        assert_command_contains(
            &manifest,
            1717,
            "python3 checks/check_workflow.py --repo . --spec-dir specs/GH1717",
        );
    }

    fn assert_command_contains(manifest: &EvalBenchmarkManifest, issue: u64, expected: &str) {
        let case = manifest
            .cases
            .iter()
            .find(|case| case.issue == issue)
            .unwrap_or_else(|| panic!("GH-{issue} case exists"));
        assert!(
            case.verify_commands
                .iter()
                .any(|command| command.contains(expected)),
            "{} missing command containing {expected}",
            case.case_id
        );
    }
}
