use chrono::{DateTime, Utc};
use harness_core::config::agents::AgentReviewConfig;
use harness_core::review::{
    parse_review_report, ReviewGateProviderReport, ReviewProviderKind, ReviewProviderRole,
};

pub(crate) type AgentReviewGateReport = Option<ReviewGateProviderReport>;

pub(crate) fn local_review_provider_report(
    review_config: &AgentReviewConfig,
    raw_output: &str,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> ReviewGateProviderReport {
    let provider_id = configured_local_review_provider_id(review_config);
    ReviewGateProviderReport {
        role: configured_review_provider_role(review_config, provider_id),
        report: parse_review_report(
            provider_id,
            configured_local_review_provider_kind(provider_id),
            raw_output,
            started_at,
            completed_at,
        ),
    }
}

fn configured_local_review_provider_id(review_config: &AgentReviewConfig) -> &'static str {
    if review_config
        .required_providers
        .iter()
        .chain(review_config.advisory_providers.iter())
        .any(|provider| provider == "codex_agent_review")
        || review_config.codex_agent_review.enabled
    {
        "codex_agent_review"
    } else {
        "codex_cli_review"
    }
}

fn configured_review_provider_role(
    review_config: &AgentReviewConfig,
    provider_id: &str,
) -> ReviewProviderRole {
    if review_config
        .required_providers
        .iter()
        .any(|provider| provider == provider_id)
    {
        ReviewProviderRole::Required
    } else {
        ReviewProviderRole::Advisory
    }
}

fn configured_local_review_provider_kind(provider_id: &str) -> ReviewProviderKind {
    match provider_id {
        "codex_cli_review" => ReviewProviderKind::LocalCli,
        _ => ReviewProviderKind::LocalAgent,
    }
}
