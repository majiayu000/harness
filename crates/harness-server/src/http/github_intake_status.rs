use harness_core::config::{
    intake::{GitHubIntakeConfig, GitHubPollDiscoveryDriver, IntakeMode},
    server::ServerConfig,
    HarnessConfig,
};
use serde_json::{json, Value};

use super::state::StoreStartupResult;

pub(crate) const GITHUB_WEBHOOK_INTAKE_SUBSYSTEM: &str = "github_webhook_intake";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitHubWebhookSecretError {
    Missing,
    Invalid,
}

impl GitHubWebhookSecretError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Missing => "missing_webhook_secret",
            Self::Invalid => "invalid_webhook_secret",
        }
    }
}

pub(crate) fn github_webhook_secret_for_request(
    server: &ServerConfig,
) -> Result<&str, GitHubWebhookSecretError> {
    match server.github_webhook_secret.as_deref() {
        None => Err(GitHubWebhookSecretError::Missing),
        Some(secret) if secret.trim().is_empty() => Err(GitHubWebhookSecretError::Invalid),
        Some(secret) => Ok(secret),
    }
}

pub(crate) fn github_intake_requires_webhook_secret(github: Option<&GitHubIntakeConfig>) -> bool {
    github
        .filter(|config| config.enabled && config.mode.webhook_autonomous())
        .is_some()
}

pub(crate) fn github_webhook_intake_startup_status(
    config: &HarnessConfig,
) -> Option<StoreStartupResult> {
    if !github_intake_requires_webhook_secret(config.intake.github.as_ref()) {
        return None;
    }
    github_webhook_secret_for_request(&config.server)
        .err()
        .map(|_| {
            StoreStartupResult::optional(GITHUB_WEBHOOK_INTAKE_SUBSYSTEM).failed(
                "server.github_webhook_secret is required for GitHub webhook-capable intake",
            )
        })
}

pub(crate) fn intake_mode_name(mode: IntakeMode) -> &'static str {
    match mode {
        IntakeMode::Poll => "poll",
        IntakeMode::Webhook => "webhook",
        IntakeMode::Hybrid => "hybrid",
    }
}

pub(crate) fn github_intake_driver_metadata(
    github: Option<&GitHubIntakeConfig>,
    server: &ServerConfig,
    active_poller_count: usize,
) -> Value {
    let enabled = github.map(|config| config.enabled).unwrap_or(false);
    let mode = github.map(|config| config.mode);
    let webhook_configured = enabled && mode.is_some_and(IntakeMode::webhook_autonomous);
    let polling_configured = enabled && mode.is_some_and(IntakeMode::poller_enabled);
    let webhook_secret_error = github_webhook_secret_for_request(server).err();
    let webhook_degraded = webhook_configured && webhook_secret_error.is_some();
    let webhook_reason = if webhook_degraded {
        webhook_secret_error.map(GitHubWebhookSecretError::reason)
    } else {
        None
    };
    let discovery_driver = github
        .map(|config| config.discovery_driver)
        .unwrap_or_default();
    let polling_active = polling_configured && active_poller_count > 0;
    let polling_degraded = polling_configured && !polling_active;
    let polling_reason = if !polling_degraded {
        None
    } else if discovery_driver == GitHubPollDiscoveryDriver::Agent {
        Some("agent_discovery_selected")
    } else {
        Some("no_active_pollers")
    };
    json!({
        "webhook": {
            "configured": webhook_configured,
            "accepting": webhook_configured && !webhook_degraded,
            "degraded": webhook_degraded,
            "reason": webhook_reason,
        },
        "polling": {
            "discovery_driver": discovery_driver.to_string(),
            "configured": polling_configured,
            "active": polling_active,
            "degraded": polling_degraded,
            "reason": polling_reason,
        },
    })
}

pub(crate) fn github_effective_repos(github: Option<&GitHubIntakeConfig>) -> Vec<Value> {
    let Some(config) = github else {
        return Vec::new();
    };
    let mode = intake_mode_name(config.mode);
    config
        .effective_repos()
        .into_iter()
        .map(|repo| {
            json!({
                "repo": repo.repo,
                "label": repo.label,
                "project_root": repo.project_root,
                "mode": mode,
                "drivers": {
                    "webhook": config.enabled && config.mode.webhook_autonomous(),
                    "polling": config.enabled && config.mode.poller_enabled(),
                    "discovery_driver": config.discovery_driver.to_string(),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github_config(mode: IntakeMode) -> GitHubIntakeConfig {
        GitHubIntakeConfig {
            enabled: true,
            mode,
            repo: "owner/repo".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn webhook_secret_for_request_rejects_missing_empty_and_whitespace() {
        let mut server = ServerConfig::default();
        assert_eq!(
            github_webhook_secret_for_request(&server),
            Err(GitHubWebhookSecretError::Missing)
        );

        server.github_webhook_secret = Some(String::new());
        assert_eq!(
            github_webhook_secret_for_request(&server),
            Err(GitHubWebhookSecretError::Invalid)
        );

        server.github_webhook_secret = Some("   ".to_string());
        assert_eq!(
            github_webhook_secret_for_request(&server),
            Err(GitHubWebhookSecretError::Invalid)
        );

        server.github_webhook_secret = Some(" secret ".to_string());
        assert_eq!(github_webhook_secret_for_request(&server), Ok(" secret "));
    }

    #[test]
    fn webhook_capable_intake_without_secret_returns_startup_degradation() {
        let mut config = HarnessConfig::default();
        config.intake.github = Some(github_config(IntakeMode::Webhook));

        let status = github_webhook_intake_startup_status(&config)
            .expect("webhook mode without secret should degrade");

        assert_eq!(status.name, GITHUB_WEBHOOK_INTAKE_SUBSYSTEM);
        assert!(!status.ready);
        assert!(!status.is_critical());
        assert!(!status.error.unwrap_or_default().contains("secret-value"));
    }

    #[test]
    fn hybrid_without_secret_degrades_but_poll_mode_and_disabled_do_not() {
        let mut config = HarnessConfig::default();
        config.intake.github = Some(github_config(IntakeMode::Hybrid));
        assert!(github_webhook_intake_startup_status(&config).is_some());

        config.intake.github = Some(github_config(IntakeMode::Poll));
        assert!(github_webhook_intake_startup_status(&config).is_none());

        let mut disabled = github_config(IntakeMode::Webhook);
        disabled.enabled = false;
        config.intake.github = Some(disabled);
        assert!(github_webhook_intake_startup_status(&config).is_none());
    }
}
