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
fn eval_credentials_preserves_allowlisted_non_secret_cloud_config() {
    let environment = build_eval_credential_environment(
        &env(&[
            ("AWS_REGION", "us-east-1"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("GOOGLE_CLOUD_PROJECT", "project-1"),
        ]),
        &allowlist(&[
            "AWS_REGION",
            "AWS_SECRET_ACCESS_KEY",
            "GOOGLE_CLOUD_PROJECT",
        ]),
        &[],
        &[],
        at(100),
    )
    .unwrap();

    assert_eq!(environment.variables()["AWS_REGION"], "us-east-1");
    assert_eq!(environment.variables()["GOOGLE_CLOUD_PROJECT"], "project-1");
    assert!(!environment
        .variables()
        .contains_key("AWS_SECRET_ACCESS_KEY"));
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
    let missing = build_eval_credential_environment(&env(&[]), &[], &[requirement()], &[], at(100))
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

    let environment = runtime_host_eval_environment(&job).unwrap().unwrap();
    let policy = environment.audit();

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

    let policy = attach_runtime_host_eval_environment_policy(&mut job)
        .unwrap()
        .unwrap();

    assert_eq!(
        job.input["command"]["eval"]["credential_environment"]["schema"],
        EVAL_CREDENTIAL_ENVIRONMENT_SCHEMA_VERSION
    );
    assert_eq!(
        job.input["command"]["eval"]["credential_environment"]["secret_inheritance"],
        "empty_by_default"
    );
    assert_eq!(policy.audit().stripped_env[0].key, "GITHUB_TOKEN");
}

#[test]
fn eval_credentials_remote_host_environment_includes_approved_grants() {
    let mut job = RuntimeJob::pending(
        "command-1",
        harness_workflow::runtime::RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "implement_issue",
            "command": {
                "eval": {
                    "eval_run_id": "run-1",
                    "credential_requirements": [{
                        "id": "github-pr-write",
                        "env_var": "GITHUB_TOKEN",
                        "scope": ["repo:owner/repo:pull_request:write"],
                        "audience": "github.com",
                        "required": true
                    }],
                    "credential_grants": [{
                        "requirement_id": "github-pr-write",
                        "env_var": "GITHUB_TOKEN",
                        "issuer": "github_app_installation",
                        "scope": ["repo:owner/repo:pull_request:write"],
                        "audience": "github.com",
                        "expires_at": "2999-01-01T00:00:00Z",
                        "value": "scoped-secret"
                    }]
                }
            }
        }),
    );
    job.id = "runtime-job-1".to_string();

    let environment = runtime_host_eval_environment(&job).unwrap().unwrap();

    assert_eq!(environment.variables()["GITHUB_TOKEN"], "scoped-secret");
    let audit_json = serde_json::to_string(environment.audit()).unwrap();
    assert!(audit_json.contains("github_app_installation"));
    assert!(!audit_json.contains("scoped-secret"));
}

#[test]
fn eval_credentials_spawn_env_is_secretless_for_eval_jobs() {
    let mut job = RuntimeJob::pending(
        "command-1",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "command": {
                "eval": {
                    "eval_run_id": "run-1",
                    "plain_env_allowlist": ["PATH", "OPENAI_API_KEY"]
                }
            }
        }),
    );
    job.id = "runtime-job-1".to_string();
    let mut env_vars = env(&[("HARNESS_AGENT_ISOLATION_TIER", "host")]);

    let audit = apply_eval_environment_to_spawn_env(&job, &mut env_vars)
        .unwrap()
        .unwrap();

    assert_eq!(env_vars["HARNESS_AGENT_SECRETLESS_ENV"], "1");
    assert!(!env_vars.contains_key("OPENAI_API_KEY"));
    assert_eq!(audit.secret_inheritance, "empty_by_default");
}
