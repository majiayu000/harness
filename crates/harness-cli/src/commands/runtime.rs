use super::{status, RuntimeBreakerCommand, RuntimeCommand};
use anyhow::Context;
use harness_core::config::HarnessConfig;
use serde_json::Value;
use std::time::Duration;

const REQUEST_TIMEOUT_SECS: u64 = 5;

pub async fn run(config: &HarnessConfig, cmd: RuntimeCommand) -> anyhow::Result<()> {
    match cmd {
        RuntimeCommand::Breaker { cmd } => match cmd {
            RuntimeBreakerCommand::Reset { profile, url } => {
                reset_breaker(config, &profile, url).await?;
            }
        },
    }
    Ok(())
}

async fn reset_breaker(
    config: &HarnessConfig,
    profile: &str,
    url: Option<String>,
) -> anyhow::Result<()> {
    let profile = profile.trim();
    if profile.is_empty() {
        anyhow::bail!("runtime profile is required");
    }
    let base_url = status::server_base_url(config, url)?;
    let mut reset_url =
        url::Url::parse(&base_url).with_context(|| format!("invalid server URL: {base_url}"))?;
    reset_url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("server URL cannot be used as a base URL"))?
        .extend(["api", "runtime", "circuit-breakers", profile, "reset"]);

    let token = status::resolve_api_token(config);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .no_proxy()
        .build()
        .context("failed to build HTTP client")?;
    let mut request = client.post(reset_url.clone());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("failed to connect to harness server at {reset_url}"))?;
    let status_code = response.status();
    if status_code == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!(
            "server rejected breaker reset with 401; set server.api_token in config or HARNESS_API_TOKEN"
        );
    }
    if !status_code.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("server returned {status_code} for breaker reset: {body}");
    }
    let body = response
        .json::<Value>()
        .await
        .context("server returned invalid JSON for breaker reset")?;
    println!("Runtime breaker reset: {profile}");
    if let Some(breakers) = body["circuit_breakers"].as_array() {
        let active = breakers
            .iter()
            .filter(|breaker| breaker["state"].as_str() != Some("closed"))
            .count();
        println!("Active circuit breakers: {active}");
    }
    Ok(())
}
