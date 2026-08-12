use chrono::{DateTime, Utc};
use harness_core::agent::AGENT_SECRETLESS_ENV_ENV;
use harness_workflow::runtime::RuntimeJob;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt;

pub(crate) const EVAL_CREDENTIAL_ENVIRONMENT_SCHEMA_VERSION: &str =
    "harness.eval.credential_environment.v1";

const DEFAULT_PLAIN_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct EvalCredentialRequirement {
    pub id: String,
    pub env_var: String,
    pub scope: Vec<String>,
    pub audience: String,
    pub required: bool,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
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
    InvalidCredentialRequirements {
        error: String,
    },
    InvalidCredentialGrants {
        error: String,
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
            Self::InvalidCredentialRequirements { error } => {
                write!(f, "invalid credential requirements: {error}")
            }
            Self::InvalidCredentialGrants { error } => {
                write!(f, "invalid credential grants: {error}")
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

pub(crate) fn eval_credential_environment_for_job(
    job: &RuntimeJob,
) -> Result<Option<EvalCredentialEnvironment>, EvalCredentialEnvironmentError> {
    if !is_eval_runtime_job(job) {
        return Ok(None);
    }
    let ambient = ambient_env_utf8();
    eval_credential_environment_for_job_with_ambient(job, &ambient).map(Some)
}

pub(crate) fn apply_eval_environment_to_spawn_env(
    job: &RuntimeJob,
    env_vars: &mut HashMap<String, String>,
) -> Result<Option<EvalCredentialEnvironmentAudit>, EvalCredentialEnvironmentError> {
    let Some(environment) = eval_credential_environment_for_job(job)? else {
        return Ok(None);
    };
    env_vars.extend(
        environment
            .variables()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    env_vars.insert(AGENT_SECRETLESS_ENV_ENV.to_string(), "1".to_string());
    Ok(Some(environment.audit().clone()))
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

pub(crate) fn runtime_host_eval_environment(
    job: &RuntimeJob,
) -> Result<Option<EvalCredentialEnvironment>, EvalCredentialEnvironmentError> {
    if !is_eval_runtime_job(job) {
        return Ok(None);
    }
    eval_credential_environment_for_job_with_ambient(job, &HashMap::new()).map(Some)
}

fn eval_credential_environment_for_job_with_ambient(
    job: &RuntimeJob,
    ambient_env: &HashMap<String, String>,
) -> Result<EvalCredentialEnvironment, EvalCredentialEnvironmentError> {
    let allowlist =
        plain_env_allowlist_from_job_input(&job.input).unwrap_or_else(default_plain_env_allowlist);
    let requirements = credential_requirements_from_job_input(&job.input)?;
    let grants = credential_grants_from_job_input(&job.input)?;
    build_eval_credential_environment(ambient_env, &allowlist, &requirements, &grants, Utc::now())
}

pub(crate) fn attach_runtime_host_eval_environment_policy(
    job: &mut RuntimeJob,
) -> Result<Option<EvalCredentialEnvironment>, EvalCredentialEnvironmentError> {
    let Some(credential_environment) = runtime_host_eval_environment(job)? else {
        return Ok(None);
    };
    attach_eval_policy_to_input(&mut job.input, credential_environment.audit());
    Ok(Some(credential_environment))
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
    if is_credential_path_env(&key) {
        return Some(EvalSecretEnvClass::Generic);
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
    matches!(
        key,
        "AWS_ACCESS_KEY_ID"
            | "AWS_SECRET_ACCESS_KEY"
            | "AWS_SESSION_TOKEN"
            | "AWS_SECURITY_TOKEN"
            | "AWS_WEB_IDENTITY_TOKEN_FILE"
            | "AZURE_CLIENT_CERTIFICATE_PATH"
            | "AZURE_CLIENT_SECRET"
            | "AZURE_FEDERATED_TOKEN_FILE"
            | "CLOUDSDK_AUTH_ACCESS_TOKEN"
            | "CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE"
            | "GCP_SERVICE_ACCOUNT_KEY"
    ) || key.ends_with("_CREDENTIALS_FILE")
        || key == "GOOGLE_APPLICATION_CREDENTIALS"
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

fn is_credential_path_env(key: &str) -> bool {
    matches!(
        key,
        "HOME"
            | "CARGO_HOME"
            | "NETRC"
            | "NPM_CONFIG_USERCONFIG"
            | "PIP_CONFIG_FILE"
            | "DOCKER_CONFIG"
            | "KUBECONFIG"
    )
}

fn is_eval_runtime_job(job: &RuntimeJob) -> bool {
    job.input.get("eval").is_some() || job.input.pointer("/command/eval").is_some()
}

fn plain_env_allowlist_from_job_input(input: &Value) -> Option<Vec<String>> {
    value_string_array(input.pointer("/eval/plain_env_allowlist"))
        .or_else(|| value_string_array(input.pointer("/command/eval/plain_env_allowlist")))
}

fn credential_requirements_from_job_input(
    input: &Value,
) -> Result<Vec<EvalCredentialRequirement>, EvalCredentialEnvironmentError> {
    parse_eval_array(input, "credential_requirements")
        .map_err(|error| EvalCredentialEnvironmentError::InvalidCredentialRequirements { error })
}

fn credential_grants_from_job_input(
    input: &Value,
) -> Result<Vec<EvalCredentialGrant>, EvalCredentialEnvironmentError> {
    parse_eval_array(input, "credential_grants")
        .map_err(|error| EvalCredentialEnvironmentError::InvalidCredentialGrants { error })
}

fn parse_eval_array<T: for<'de> Deserialize<'de>>(
    input: &Value,
    field: &str,
) -> Result<Vec<T>, String> {
    let pointers = [format!("/eval/{field}"), format!("/command/eval/{field}")];
    let Some(value) = pointers.iter().find_map(|pointer| input.pointer(pointer)) else {
        return Ok(Vec::new());
    };
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
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

fn ambient_env_utf8() -> HashMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((os_string_to_string(key)?, os_string_to_string(value)?)))
        .collect()
}

fn os_string_to_string(value: OsString) -> Option<String> {
    value.into_string().ok()
}

#[cfg(test)]
#[path = "eval_credentials_tests.rs"]
mod eval_credentials_tests;
