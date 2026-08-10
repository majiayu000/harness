use chrono::{DateTime, Utc};
use harness_workflow::runtime::RuntimeJob;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

pub(crate) const EVAL_CREDENTIAL_ENVIRONMENT_SCHEMA_VERSION: &str =
    "harness.eval.credential_environment.v1";

const DEFAULT_PLAIN_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "CI",
    "TERM",
    "LANG",
    "LC_ALL",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvalSecretEnvClass {
    Provider,
    #[serde(rename = "github")]
    GitHub,
    Cloud,
    Ssh,
    Wrapper,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StrippedEvalEnvKey {
    pub key: String,
    pub class: EvalSecretEnvClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvalCredentialRequirement {
    pub id: String,
    pub env_var: String,
    pub scope: Vec<String>,
    pub audience: String,
    pub required: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EvalCredentialGrant {
    pub requirement_id: String,
    pub env_var: String,
    pub issuer: String,
    pub scope: Vec<String>,
    pub audience: String,
    pub expires_at: DateTime<Utc>,
    pub value: String,
}

impl fmt::Debug for EvalCredentialGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvalCredentialGrant")
            .field("requirement_id", &self.requirement_id)
            .field("env_var", &self.env_var)
            .field("issuer", &self.issuer)
            .field("scope", &self.scope)
            .field("audience", &self.audience)
            .field("expires_at", &self.expires_at)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvalCredentialGrantRecord {
    pub requirement_id: String,
    pub env_var: String,
    pub issuer: String,
    pub scope: Vec<String>,
    pub audience: String,
    pub expires_at: DateTime<Utc>,
}

impl From<&EvalCredentialGrant> for EvalCredentialGrantRecord {
    fn from(grant: &EvalCredentialGrant) -> Self {
        Self {
            requirement_id: grant.requirement_id.clone(),
            env_var: normalize_env_key(&grant.env_var),
            issuer: grant.issuer.trim().to_string(),
            scope: normalize_scope(&grant.scope),
            audience: grant.audience.trim().to_string(),
            expires_at: grant.expires_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvalCredentialEnvironmentAudit {
    pub schema: &'static str,
    pub secret_inheritance: &'static str,
    pub plain_env_allowlist: Vec<String>,
    pub plain_env_keys: Vec<String>,
    pub stripped_env: Vec<StrippedEvalEnvKey>,
    pub credential_grants: Vec<EvalCredentialGrantRecord>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EvalCredentialEnvironment {
    variables: BTreeMap<String, String>,
    audit: EvalCredentialEnvironmentAudit,
}

impl EvalCredentialEnvironment {
    pub(crate) fn variables(&self) -> &BTreeMap<String, String> {
        &self.variables
    }

    pub(crate) fn audit(&self) -> &EvalCredentialEnvironmentAudit {
        &self.audit
    }
}

impl fmt::Debug for EvalCredentialEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvalCredentialEnvironment")
            .field("variable_keys", &self.variables.keys().collect::<Vec<_>>())
            .field("audit", &self.audit)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvalCredentialEnvironmentError {
    DuplicateRequirement {
        requirement_id: String,
    },
    UndeclaredCredentialGrant {
        requirement_id: String,
    },
    MissingRequiredCredentialGrant {
        requirement_id: String,
    },
    GrantEnvVarMismatch {
        requirement_id: String,
        expected: String,
        actual: String,
    },
    GrantScopeMismatch {
        requirement_id: String,
    },
    GrantAudienceMismatch {
        requirement_id: String,
    },
    ExpiredGrant {
        requirement_id: String,
        expires_at: DateTime<Utc>,
    },
    DuplicateCredentialEnvVar {
        env_var: String,
    },
}

impl fmt::Display for EvalCredentialEnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateRequirement { requirement_id } => {
                write!(f, "duplicate credential requirement `{requirement_id}`")
            }
            Self::UndeclaredCredentialGrant { requirement_id } => {
                write!(
                    f,
                    "credential grant references undeclared requirement `{requirement_id}`"
                )
            }
            Self::MissingRequiredCredentialGrant { requirement_id } => {
                write!(
                    f,
                    "required credential requirement `{requirement_id}` has no approved grant"
                )
            }
            Self::GrantEnvVarMismatch {
                requirement_id,
                expected,
                actual,
            } => write!(
                f,
                "credential grant `{requirement_id}` targets env var `{actual}` but requirement declares `{expected}`"
            ),
            Self::GrantScopeMismatch { requirement_id } => {
                write!(f, "credential grant `{requirement_id}` scope is not approved")
            }
            Self::GrantAudienceMismatch { requirement_id } => {
                write!(
                    f,
                    "credential grant `{requirement_id}` audience is not approved"
                )
            }
            Self::ExpiredGrant {
                requirement_id,
                expires_at,
            } => write!(
                f,
                "credential grant `{requirement_id}` expired at {expires_at}"
            ),
            Self::DuplicateCredentialEnvVar { env_var } => {
                write!(f, "multiple credential grants target env var `{env_var}`")
            }
        }
    }
}

impl std::error::Error for EvalCredentialEnvironmentError {}

pub(crate) fn default_plain_env_allowlist() -> Vec<String> {
    DEFAULT_PLAIN_ENV_ALLOWLIST
        .iter()
        .map(|key| key.to_string())
        .collect()
}

pub(crate) fn build_default_eval_command_environment() -> EvalCredentialEnvironment {
    let ambient = std::env::vars().collect::<HashMap<_, _>>();
    build_eval_credential_environment(
        &ambient,
        &default_plain_env_allowlist(),
        &[],
        &[],
        Utc::now(),
    )
    .expect("default eval command environment has no credential grants")
}

pub(crate) fn build_eval_credential_environment(
    ambient_env: &HashMap<String, String>,
    plain_env_allowlist: &[String],
    requirements: &[EvalCredentialRequirement],
    grants: &[EvalCredentialGrant],
    now: DateTime<Utc>,
) -> Result<EvalCredentialEnvironment, EvalCredentialEnvironmentError> {
    let plain_env_allowlist = normalize_allowlist(plain_env_allowlist);
    let mut variables = BTreeMap::new();
    let mut stripped_env = BTreeMap::<String, EvalSecretEnvClass>::new();

    for key in &plain_env_allowlist {
        if let Some(class) = secret_env_class(key) {
            stripped_env.insert(key.clone(), class);
            continue;
        }
        if let Some(value) = ambient_env.get(key) {
            variables.insert(key.clone(), value.clone());
        }
    }

    let requirements_by_id = requirements_by_id(requirements)?;
    let mut granted_requirement_ids = BTreeSet::new();
    let mut grant_records = Vec::new();
    for grant in grants {
        let requirement_id = grant.requirement_id.trim().to_string();
        let Some(requirement) = requirements_by_id.get(requirement_id.as_str()) else {
            return Err(EvalCredentialEnvironmentError::UndeclaredCredentialGrant {
                requirement_id,
            });
        };
        validate_grant(requirement, grant, now)?;
        let env_var = normalize_env_key(&grant.env_var);
        if variables
            .insert(env_var.clone(), grant.value.clone())
            .is_some()
        {
            return Err(EvalCredentialEnvironmentError::DuplicateCredentialEnvVar { env_var });
        }
        granted_requirement_ids.insert(requirement_id);
        grant_records.push(EvalCredentialGrantRecord::from(grant));
    }

    for requirement in requirements {
        let requirement_id = requirement.id.trim();
        if requirement.required && !granted_requirement_ids.contains(requirement_id) {
            return Err(
                EvalCredentialEnvironmentError::MissingRequiredCredentialGrant {
                    requirement_id: requirement_id.to_string(),
                },
            );
        }
    }

    let plain_env_keys = variables
        .keys()
        .filter(|key| {
            grants
                .iter()
                .all(|grant| normalize_env_key(&grant.env_var) != **key)
        })
        .cloned()
        .collect();
    let stripped_env = stripped_env
        .into_iter()
        .map(|(key, class)| StrippedEvalEnvKey { key, class })
        .collect();
    Ok(EvalCredentialEnvironment {
        variables,
        audit: EvalCredentialEnvironmentAudit {
            schema: EVAL_CREDENTIAL_ENVIRONMENT_SCHEMA_VERSION,
            secret_inheritance: "empty_by_default",
            plain_env_allowlist,
            plain_env_keys,
            stripped_env,
            credential_grants: grant_records,
        },
    })
}

pub(crate) fn runtime_host_eval_environment_policy(
    job: &RuntimeJob,
) -> Option<EvalCredentialEnvironmentAudit> {
    if !is_eval_runtime_job(job) {
        return None;
    }
    let allowlist =
        plain_env_allowlist_from_job_input(&job.input).unwrap_or_else(default_plain_env_allowlist);
    let environment =
        build_eval_credential_environment(&HashMap::new(), &allowlist, &[], &[], Utc::now())
            .expect("runtime-host eval policy has no credential grants");
    Some(environment.audit().clone())
}

pub(crate) fn attach_runtime_host_eval_environment_policy(
    job: &mut RuntimeJob,
) -> Option<EvalCredentialEnvironmentAudit> {
    let credential_environment = runtime_host_eval_environment_policy(job)?;
    attach_eval_policy_to_input(&mut job.input, &credential_environment);
    Some(credential_environment)
}

fn requirements_by_id(
    requirements: &[EvalCredentialRequirement],
) -> Result<BTreeMap<&str, &EvalCredentialRequirement>, EvalCredentialEnvironmentError> {
    let mut by_id = BTreeMap::new();
    for requirement in requirements {
        let requirement_id = requirement.id.trim();
        if by_id.insert(requirement_id, requirement).is_some() {
            return Err(EvalCredentialEnvironmentError::DuplicateRequirement {
                requirement_id: requirement_id.to_string(),
            });
        }
    }
    Ok(by_id)
}

fn validate_grant(
    requirement: &EvalCredentialRequirement,
    grant: &EvalCredentialGrant,
    now: DateTime<Utc>,
) -> Result<(), EvalCredentialEnvironmentError> {
    let requirement_id = requirement.id.trim().to_string();
    let expected_env = normalize_env_key(&requirement.env_var);
    let actual_env = normalize_env_key(&grant.env_var);
    if expected_env != actual_env {
        return Err(EvalCredentialEnvironmentError::GrantEnvVarMismatch {
            requirement_id,
            expected: expected_env,
            actual: actual_env,
        });
    }
    if normalize_scope(&requirement.scope) != normalize_scope(&grant.scope) {
        return Err(EvalCredentialEnvironmentError::GrantScopeMismatch { requirement_id });
    }
    if requirement.audience.trim() != grant.audience.trim() {
        return Err(EvalCredentialEnvironmentError::GrantAudienceMismatch { requirement_id });
    }
    if grant.expires_at <= now {
        return Err(EvalCredentialEnvironmentError::ExpiredGrant {
            requirement_id,
            expires_at: grant.expires_at,
        });
    }
    Ok(())
}

fn normalize_allowlist(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| normalize_env_key(value))
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_scope(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_env_key(key: &str) -> String {
    key.trim().to_string()
}

pub(crate) fn secret_env_class(key: &str) -> Option<EvalSecretEnvClass> {
    let key = key.trim().to_ascii_uppercase();
    if key.is_empty() {
        return None;
    }
    if is_wrapper_env(&key) {
        return Some(EvalSecretEnvClass::Wrapper);
    }
    if is_provider_env(&key) {
        return Some(EvalSecretEnvClass::Provider);
    }
    if is_github_env(&key) {
        return Some(EvalSecretEnvClass::GitHub);
    }
    if is_cloud_env(&key) {
        return Some(EvalSecretEnvClass::Cloud);
    }
    if is_ssh_env(&key) {
        return Some(EvalSecretEnvClass::Ssh);
    }
    if is_generic_secret_env(&key) {
        return Some(EvalSecretEnvClass::Generic);
    }
    None
}

fn is_wrapper_env(key: &str) -> bool {
    matches!(
        key,
        "CLAUDECODE"
            | "CLAUDE_CODE"
            | "CLAUDE_CODE_ENTRYPOINT"
            | "CLAUDE_CODE_SESSION_ID"
            | "CLAUDE_SESSION_ID"
    ) || key.starts_with("CODEX_")
}

fn is_provider_env(key: &str) -> bool {
    matches!(
        key,
        "OPENAI_API_KEY"
            | "ANTHROPIC_API_KEY"
            | "AZURE_OPENAI_API_KEY"
            | "GOOGLE_API_KEY"
            | "GEMINI_API_KEY"
            | "MISTRAL_API_KEY"
            | "COHERE_API_KEY"
            | "OPENROUTER_API_KEY"
            | "PERPLEXITY_API_KEY"
            | "XAI_API_KEY"
            | "TOGETHER_API_KEY"
            | "GROQ_API_KEY"
    )
}

fn is_github_env(key: &str) -> bool {
    key == "GITHUB_TOKEN"
        || key == "GH_TOKEN"
        || key == "GITHUB_APP_PRIVATE_KEY"
        || key == "GITHUB_WEBHOOK_SECRET"
        || key == "GITHUB_CLIENT_SECRET"
        || (key.starts_with("GITHUB_") && is_generic_secret_env(key))
}

fn is_cloud_env(key: &str) -> bool {
    key.starts_with("AWS_")
        || key.starts_with("AZURE_")
        || key.starts_with("GCLOUD_")
        || key.starts_with("GCP_")
        || key.starts_with("CLOUDSDK_")
        || key == "GOOGLE_APPLICATION_CREDENTIALS"
        || key == "GOOGLE_CLOUD_PROJECT"
        || key == "KUBECONFIG"
        || key == "DOCKER_CONFIG"
}

fn is_ssh_env(key: &str) -> bool {
    key == "SSH_AUTH_SOCK"
        || key == "SSH_AGENT_PID"
        || key == "GIT_SSH"
        || key == "GIT_SSH_COMMAND"
        || key.starts_with("SSH_")
}

fn is_generic_secret_env(key: &str) -> bool {
    key.ends_with("_TOKEN")
        || key.contains("API_KEY")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
        || key.contains("PRIVATE_KEY")
        || key.contains("CREDENTIAL")
}

fn is_eval_runtime_job(job: &RuntimeJob) -> bool {
    job.input.get("eval").is_some() || job.input.pointer("/command/eval").is_some()
}

fn plain_env_allowlist_from_job_input(input: &Value) -> Option<Vec<String>> {
    value_string_array(input.pointer("/eval/plain_env_allowlist"))
        .or_else(|| value_string_array(input.pointer("/command/eval/plain_env_allowlist")))
}

fn attach_eval_policy_to_input(input: &mut Value, policy: &EvalCredentialEnvironmentAudit) {
    let policy_value =
        serde_json::to_value(policy).expect("credential environment audit serializes");
    if let Some(eval) = input.get_mut("eval").and_then(Value::as_object_mut) {
        eval.insert("credential_environment".to_string(), policy_value.clone());
    }
    if let Some(eval) = input
        .pointer_mut("/command/eval")
        .and_then(Value::as_object_mut)
    {
        eval.insert("credential_environment".to_string(), policy_value);
    }
}

fn value_string_array(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    Some(
        values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn allowlist(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().unwrap()
    }

    fn requirement() -> EvalCredentialRequirement {
        EvalCredentialRequirement {
            id: "github-pr-write".to_string(),
            env_var: "GITHUB_TOKEN".to_string(),
            scope: vec!["repo:owner/repo:pull_request:write".to_string()],
            audience: "github.com".to_string(),
            required: true,
        }
    }

    fn grant(value: &str, expires_at: DateTime<Utc>) -> EvalCredentialGrant {
        EvalCredentialGrant {
            requirement_id: "github-pr-write".to_string(),
            env_var: "GITHUB_TOKEN".to_string(),
            issuer: "github_app_installation".to_string(),
            scope: vec!["repo:owner/repo:pull_request:write".to_string()],
            audience: "github.com".to_string(),
            expires_at,
            value: value.to_string(),
        }
    }

    #[test]
    fn eval_credentials_builds_environment_from_plain_allowlist() {
        let environment = build_eval_credential_environment(
            &env(&[("PATH", "/bin"), ("SAFE_FLAG", "1"), ("UNLISTED", "hidden")]),
            &allowlist(&["PATH", "SAFE_FLAG"]),
            &[],
            &[],
            at(100),
        )
        .unwrap();

        assert_eq!(environment.variables()["PATH"], "/bin");
        assert_eq!(environment.variables()["SAFE_FLAG"], "1");
        assert!(!environment.variables().contains_key("UNLISTED"));
        assert_eq!(environment.audit().plain_env_keys, ["PATH", "SAFE_FLAG"]);
    }

    #[test]
    fn eval_credentials_strips_provider_github_cloud_ssh_and_wrapper_secrets() {
        let environment = build_eval_credential_environment(
            &env(&[
                ("OPENAI_API_KEY", "sk-secret-provider"),
                ("GITHUB_TOKEN", "ghp-secret"),
                ("AWS_ACCESS_KEY_ID", "cloud-secret"),
                ("SSH_AUTH_SOCK", "/tmp/ssh.sock"),
                ("CLAUDE_CODE_ENTRYPOINT", "wrapper"),
                ("SAFE_FLAG", "ok"),
            ]),
            &allowlist(&[
                "OPENAI_API_KEY",
                "GITHUB_TOKEN",
                "AWS_ACCESS_KEY_ID",
                "SSH_AUTH_SOCK",
                "CLAUDE_CODE_ENTRYPOINT",
                "SAFE_FLAG",
            ]),
            &[],
            &[],
            at(100),
        )
        .unwrap();

        assert_eq!(environment.variables().len(), 1);
        assert_eq!(environment.variables()["SAFE_FLAG"], "ok");
        let stripped = environment
            .audit()
            .stripped_env
            .iter()
            .map(|item| (item.key.as_str(), item.class))
            .collect::<Vec<_>>();
        assert_eq!(
            stripped,
            [
                ("AWS_ACCESS_KEY_ID", EvalSecretEnvClass::Cloud),
                ("CLAUDE_CODE_ENTRYPOINT", EvalSecretEnvClass::Wrapper),
                ("GITHUB_TOKEN", EvalSecretEnvClass::GitHub),
                ("OPENAI_API_KEY", EvalSecretEnvClass::Provider),
                ("SSH_AUTH_SOCK", EvalSecretEnvClass::Ssh),
            ]
        );
    }

    #[test]
    fn eval_credentials_records_grant_metadata_without_secret_value() {
        let secret = "ghs_very_secret_runtime_token";
        let requirement = requirement();
        let grant = grant(secret, at(200));
        let environment = build_eval_credential_environment(
            &env(&[("PATH", "/bin")]),
            &allowlist(&["PATH"]),
            std::slice::from_ref(&requirement),
            std::slice::from_ref(&grant),
            at(100),
        )
        .unwrap();

        assert_eq!(environment.variables()["GITHUB_TOKEN"], secret);
        let audit_json = serde_json::to_string(environment.audit()).unwrap();
        assert!(audit_json.contains("github_app_installation"));
        assert!(audit_json.contains("repo:owner/repo:pull_request:write"));
        assert!(audit_json.contains("github.com"));
        assert!(!audit_json.contains(secret));
        assert!(!format!("{environment:?}").contains(secret));
        assert!(!format!("{grant:?}").contains(secret));
    }

    #[test]
    fn eval_credentials_rejects_expired_scope_and_audience_mismatched_grants() {
        let requirement = requirement();

        let expired = build_eval_credential_environment(
            &env(&[]),
            &[],
            std::slice::from_ref(&requirement),
            &[grant("secret", at(100))],
            at(100),
        )
        .unwrap_err();
        assert!(matches!(
            expired,
            EvalCredentialEnvironmentError::ExpiredGrant { .. }
        ));

        let mut wrong_scope = grant("secret", at(200));
        wrong_scope.scope = vec!["repo:owner/repo:contents:write".to_string()];
        let scope_error = build_eval_credential_environment(
            &env(&[]),
            &[],
            std::slice::from_ref(&requirement),
            &[wrong_scope],
            at(100),
        )
        .unwrap_err();
        assert!(matches!(
            scope_error,
            EvalCredentialEnvironmentError::GrantScopeMismatch { .. }
        ));

        let mut wrong_audience = grant("secret", at(200));
        wrong_audience.audience = "api.github.com".to_string();
        let audience_error = build_eval_credential_environment(
            &env(&[]),
            &[],
            std::slice::from_ref(&requirement),
            &[wrong_audience],
            at(100),
        )
        .unwrap_err();
        assert!(matches!(
            audience_error,
            EvalCredentialEnvironmentError::GrantAudienceMismatch { .. }
        ));
    }

    #[test]
    fn eval_credentials_rejects_undeclared_or_missing_credential_requirements() {
        let missing =
            build_eval_credential_environment(&env(&[]), &[], &[requirement()], &[], at(100))
                .unwrap_err();
        assert!(matches!(
            missing,
            EvalCredentialEnvironmentError::MissingRequiredCredentialGrant { .. }
        ));

        let undeclared = build_eval_credential_environment(
            &env(&[]),
            &[],
            &[],
            &[grant("secret", at(200))],
            at(100),
        )
        .unwrap_err();
        assert!(matches!(
            undeclared,
            EvalCredentialEnvironmentError::UndeclaredCredentialGrant { .. }
        ));
    }

    #[test]
    fn eval_credentials_runtime_host_policy_marks_eval_jobs_secretless() {
        let mut job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "plain_env_allowlist": ["SAFE_FLAG", "GITHUB_TOKEN"]
                    }
                }
            }),
        );
        job.id = "runtime-job-1".to_string();

        let policy = runtime_host_eval_environment_policy(&job).unwrap();

        assert_eq!(policy.secret_inheritance, "empty_by_default");
        assert_eq!(policy.plain_env_allowlist, ["GITHUB_TOKEN", "SAFE_FLAG"]);
        assert!(policy.plain_env_keys.is_empty());
        assert_eq!(policy.stripped_env.len(), 1);
        assert_eq!(policy.stripped_env[0].key, "GITHUB_TOKEN");
        assert_eq!(policy.stripped_env[0].class, EvalSecretEnvClass::GitHub);
    }

    #[test]
    fn eval_credentials_attaches_policy_to_eval_runtime_job_payload() {
        let mut job = RuntimeJob::pending(
            "command-1",
            harness_workflow::runtime::RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "plain_env_allowlist": ["PATH", "GITHUB_TOKEN"]
                    }
                }
            }),
        );
        job.id = "runtime-job-1".to_string();

        let policy = attach_runtime_host_eval_environment_policy(&mut job).unwrap();

        assert_eq!(
            job.input["command"]["eval"]["credential_environment"]["schema"],
            EVAL_CREDENTIAL_ENVIRONMENT_SCHEMA_VERSION
        );
        assert_eq!(
            job.input["command"]["eval"]["credential_environment"]["secret_inheritance"],
            "empty_by_default"
        );
        assert_eq!(policy.stripped_env[0].key, "GITHUB_TOKEN");
    }
}
