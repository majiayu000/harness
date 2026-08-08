use super::{
    is_nested_session_env, is_spawn_control_env, DEFAULT_AGENT_CONTAINER_IMAGE,
    REVIEW_GIT_SAFE_WORKSPACE_ENV,
};
use crate::scoped_token::{
    CONTAINER_GH_TOKEN_ENV, CONTAINER_GITHUB_TOKEN_ENV, SCOPED_GITHUB_TOKEN_ENV,
};
use harness_core::agent::{
    AGENT_CONTAINER_IMAGE_ENV, AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV,
};
use harness_core::config::isolation::IsolationTier;
use harness_core::error::HarnessError;
use harness_core::run_id::{AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};
use std::collections::{BTreeMap, HashMap};

pub(super) fn isolation_tier(
    env_vars: &HashMap<String, String>,
) -> Result<IsolationTier, HarnessError> {
    match env_vars.get(AGENT_ISOLATION_TIER_ENV).map(String::as_str) {
        None | Some("") | Some("host") => Ok(IsolationTier::Host),
        Some("container") => Ok(IsolationTier::Container),
        Some("microvm") => Ok(IsolationTier::Microvm),
        Some(other) => Err(HarnessError::AgentExecution(format!(
            "unknown isolation tier `{other}`"
        ))),
    }
}

pub(super) fn network_allowlist(env_vars: &HashMap<String, String>) -> Vec<String> {
    env_vars
        .get(AGENT_NETWORK_ALLOWLIST_ENV)
        .map(String::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub(super) fn host_process_env(env_vars: &HashMap<String, String>) -> BTreeMap<String, String> {
    env_vars
        .iter()
        .filter(|(key, _)| !is_spawn_control_env(key))
        .filter(|(key, _)| !is_nested_session_env(key))
        .filter(|(key, _)| key.as_str() != SCOPED_GITHUB_TOKEN_ENV)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub(super) struct ContainerEnv {
    pub(super) plain: BTreeMap<String, String>,
    pub(super) secret: BTreeMap<String, String>,
}

pub(super) fn container_env_vars(env_vars: &HashMap<String, String>) -> ContainerEnv {
    let plain = env_vars
        .iter()
        .filter(|(key, _)| !is_spawn_control_env(key))
        .filter(|(key, _)| !is_nested_session_env(key))
        .filter(|(key, _)| key.as_str() != SCOPED_GITHUB_TOKEN_ENV)
        .filter(|(key, _)| !is_operator_secret_env(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut secret = BTreeMap::new();
    if let Some(scoped_token) = env_vars
        .get(SCOPED_GITHUB_TOKEN_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        secret.insert(
            CONTAINER_GITHUB_TOKEN_ENV.to_string(),
            scoped_token.to_string(),
        );
        secret.insert(CONTAINER_GH_TOKEN_ENV.to_string(), scoped_token.to_string());
    }
    ContainerEnv { plain, secret }
}

pub(super) fn docker_process_env(secret: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = harness_core::config::process_env::var("PATH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|path| BTreeMap::from([("PATH".to_string(), path)]))
        .unwrap_or_default();
    env.extend(secret);
    env
}

pub(super) fn review_git_safe_workspace(env_vars: &HashMap<String, String>) -> bool {
    env_vars
        .get(REVIEW_GIT_SAFE_WORKSPACE_ENV)
        .is_some_and(|value| value == "1")
}

fn is_operator_secret_env(key: &str) -> bool {
    if key == AGENT_RUN_ID_ENV || key == AGENT_RUN_PARENT_ENV || key.starts_with("HARNESS_SCOPED_")
    {
        return false;
    }
    let key = key.to_ascii_uppercase();
    key == "GITHUB_TOKEN"
        || key == "GH_TOKEN"
        || key.ends_with("_TOKEN")
        || key.contains("API_KEY")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
}

pub(super) fn container_image(env_vars: &HashMap<String, String>) -> String {
    env_vars
        .get(AGENT_CONTAINER_IMAGE_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_AGENT_CONTAINER_IMAGE)
        .to_string()
}
