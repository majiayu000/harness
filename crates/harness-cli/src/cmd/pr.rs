use harness_agents::codex::{CodexAgent, CodexReviewRequest};
use harness_core::{
    agent::{AgentRequest, CodeAgent, AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV},
    config::{agents::SandboxMode, HarnessConfig},
    prompts,
    review::{parse_review_report, ReviewDecision, ReviewProviderKind},
};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};

const CODEX_CLI_REVIEW_PROVIDER_ID: &str = "codex_cli_review";
const CODEX_REVIEW_PROCESS_SPAWN_CONTROL_ENV: [&str; 2] = [
    "HARNESS_AGENT_CONTAINER_IMAGE",
    "HARNESS_AGENT_EGRESS_PROXY_IMAGE",
];

fn codex_review_spawn_env(config: &HarnessConfig) -> HashMap<String, String> {
    codex_review_spawn_env_with(config, |key| std::env::var(key).ok())
}

fn codex_review_spawn_env_with(
    config: &HarnessConfig,
    mut read_process_env: impl FnMut(&str) -> Option<String>,
) -> HashMap<String, String> {
    let mut env_vars = HashMap::from([(
        AGENT_ISOLATION_TIER_ENV.to_string(),
        config.isolation.default_tier.as_str().to_string(),
    )]);
    let allowlist = config
        .isolation
        .network_allowlist
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    if !allowlist.is_empty() {
        env_vars.insert(AGENT_NETWORK_ALLOWLIST_ENV.to_string(), allowlist);
    }
    for key in CODEX_REVIEW_PROCESS_SPAWN_CONTROL_ENV {
        if let Some(value) = read_process_env(key).filter(|value| !value.trim().is_empty()) {
            env_vars.insert(key.to_string(), value);
        }
    }
    env_vars
}

fn create_agent(config: &HarnessConfig) -> impl CodeAgent {
    // This path needs the Claude backend alone, not a registry — but it takes
    // it from the same builder, so it cannot drift from the entry points.
    harness_agents::builder::claude_agent_from_config(&config.agents, config.agents.sandbox_mode)
}

pub async fn fix(
    config: &HarnessConfig,
    issue: u64,
    wait: u64,
    max_rounds: u32,
    project: PathBuf,
) -> anyhow::Result<()> {
    let agent = create_agent(config);

    println!("[harness] Round 1 — Implementing issue #{issue} and creating PR");

    let req = AgentRequest::from_prompt_layers(
        prompts::implement_from_issue(issue, None, None).into(),
        project.clone(),
    );

    let resp = agent.execute(req).await?;
    println!("{}", resp.output);

    let pr_url = prompts::parse_pr_url(&resp.output)
        .ok_or_else(|| anyhow::anyhow!("PR_URL=<url> not found in agent output"))?;
    let pr_number = prompts::extract_pr_number(&pr_url)
        .ok_or_else(|| anyhow::anyhow!("Cannot parse PR number from URL: {pr_url}"))?;

    println!("[harness] PR #{pr_number} created: {pr_url}");

    run_review_loop(
        &agent,
        &project,
        ReviewLoopOptions {
            issue: Some(issue),
            pr: pr_number,
            pr_url: Some(&pr_url),
            review_bot_command: &config.agents.review.review_bot_command,
            reviewer_name: &config.agents.review.reviewer_name,
            wait,
            max_rounds,
        },
    )
    .await
}

pub async fn loop_pr(
    config: &HarnessConfig,
    pr: u64,
    wait: u64,
    max_rounds: u32,
    project: PathBuf,
) -> anyhow::Result<()> {
    let agent = create_agent(config);

    println!("[harness] Starting review loop for PR #{pr}");

    run_review_loop(
        &agent,
        &project,
        ReviewLoopOptions {
            issue: None,
            pr,
            pr_url: None,
            review_bot_command: &config.agents.review.review_bot_command,
            reviewer_name: &config.agents.review.reviewer_name,
            wait,
            max_rounds,
        },
    )
    .await
}

pub async fn review(
    config: &HarnessConfig,
    pr: u64,
    provider: String,
    base: Option<String>,
    project: PathBuf,
) -> anyhow::Result<()> {
    if provider != CODEX_CLI_REVIEW_PROVIDER_ID {
        anyhow::bail!(
            "unsupported review provider `{provider}`; supported provider: {CODEX_CLI_REVIEW_PROVIDER_ID}"
        );
    }

    let mut review_config = config.agents.review.codex_cli_review.clone();
    if let Some(base) = base {
        review_config.base_ref = base;
    }
    if review_config.base_ref.trim().is_empty() {
        anyhow::bail!("codex_cli_review requires a non-empty base ref");
    }

    #[allow(clippy::disallowed_methods)]
    let agent = CodexAgent::new(
        review_config.cli_path.clone(),
        SandboxMode::ReadOnlyWithNetwork,
    )
    .with_stream_timeout(Some(review_config.timeout_secs));

    let started_at = chrono::Utc::now();
    let response = tokio::time::timeout(
        Duration::from_secs(review_config.timeout_secs.max(1)),
        agent.execute_review(CodexReviewRequest {
            project_root: project,
            instructions: Some(codex_cli_review_instructions(
                pr,
                &review_config.base_ref,
                &review_config.output_format,
            )),
            base_ref: Some(review_config.base_ref.clone()),
            model: Some(review_config.model),
            reasoning_effort: Some(review_config.reasoning_effort),
            sandbox_mode: SandboxMode::ReadOnlyWithNetwork,
            approval_policy: Some("never".to_string()),
            permission_mode: config.agents.resolve_permission_mode(),
            env_vars: codex_review_spawn_env(config),
        }),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "codex_cli_review timed out after {}s",
            review_config.timeout_secs.max(1)
        )
    })??;
    let completed_at = chrono::Utc::now();

    let report = parse_review_report(
        CODEX_CLI_REVIEW_PROVIDER_ID,
        ReviewProviderKind::LocalCli,
        &response.output,
        started_at,
        completed_at,
    );
    println!("{}", serde_json::to_string_pretty(&report)?);

    match report.decision {
        ReviewDecision::Approved => Ok(()),
        ReviewDecision::ChangesRequested => {
            anyhow::bail!("codex_cli_review requested changes for PR #{pr}")
        }
        ReviewDecision::Failed | ReviewDecision::TimedOut | ReviewDecision::Skipped => {
            anyhow::bail!(
                "codex_cli_review did not approve PR #{pr}: {}",
                report.summary
            )
        }
    }
}

fn codex_cli_review_instructions(pr: u64, base_ref: &str, output_format: &str) -> String {
    let report_format = if output_format.eq_ignore_ascii_case("json") {
        "Return exactly one fenced `harness-review-report` JSON block with this shape:\n\
         ```harness-review-report\n\
         {\"decision\":\"approved|changes_requested|failed|timed_out|skipped\",\
         \"summary\":\"concise summary\",\
         \"findings\":[{\"severity\":\"critical|high|medium|low\",\
         \"category\":\"security|correctness|data_integrity|concurrency|performance|test_gap|maintainability|other\",\
         \"path\":\"optional path or null\",\
         \"line\":123,\
         \"message\":\"finding\",\
         \"evidence\":\"optional evidence or null\",\
         \"recommendation\":\"optional recommendation or null\",\
         \"blocking\":true,\
         \"confidence\":0.9}]}\n\
         ```"
    } else {
        "If everything is safe, put APPROVED on the last line. Otherwise list each blocking issue on its own line prefixed with ISSUE:."
    };

    format!(
        "You are the configured codex_cli_review provider for PR #{pr}.\n\
         Review the local workspace against base ref `{base_ref}` as a read-only provider.\n\n\
         Requirements:\n\
         - Inspect the PR intent, diff, and changed files before deciding.\n\
         - Focus on security, logic, data integrity, error handling, and missing tests.\n\
         - Do not modify files, commit, push, or post GitHub comments.\n\
         - Treat unparseable or incomplete evidence as a failed provider review.\n\
         {report_format}"
    )
}

/// Resolve `owner/repo` slug from a PR URL.
async fn resolve_repo_slug(pr: u64, pr_url: Option<&str>) -> anyhow::Result<String> {
    if let Some(url) = pr_url {
        return Ok(prompts::repo_slug_from_pr_url(Some(url)));
    }

    Err(anyhow::anyhow!(
        "PR URL is required for PR #{pr}; Harness CLI no longer invokes gh to resolve repository metadata"
    ))
}

struct ReviewLoopOptions<'a> {
    issue: Option<u64>,
    pr: u64,
    pr_url: Option<&'a str>,
    review_bot_command: &'a str,
    reviewer_name: &'a str,
    wait: u64,
    max_rounds: u32,
}

async fn run_review_loop(
    agent: &impl CodeAgent,
    project: &PathBuf,
    options: ReviewLoopOptions<'_>,
) -> anyhow::Result<()> {
    let ReviewLoopOptions {
        issue,
        pr,
        pr_url,
        review_bot_command,
        reviewer_name,
        wait,
        max_rounds,
    } = options;
    let url_display: std::borrow::Cow<str> = match pr_url {
        Some(url) => std::borrow::Cow::Borrowed(url),
        None => std::borrow::Cow::Owned(format!("PR #{pr}")),
    };

    // Resolve once before entering the loop so failures surface immediately.
    let repo = resolve_repo_slug(pr, pr_url).await?;

    let mut prev_fixed = false;
    let mut round = 1u32;

    while round <= max_rounds {
        println!("[harness] Waiting {wait}s for CI and review bot...");
        sleep(Duration::from_secs(wait)).await;

        println!("[harness] Review round {round}/{max_rounds}, PR #{pr}");

        let req = AgentRequest {
            prompt: prompts::review_prompt(
                issue,
                pr,
                round,
                prev_fixed,
                review_bot_command,
                reviewer_name,
                &repo,
                false,
            ),
            project_root: project.clone(),
            ..Default::default()
        };

        let resp = agent.execute(req).await?;
        println!("{}", resp.output);

        if prompts::is_waiting(&resp.output) {
            println!("[harness] Review bot hasn't re-reviewed yet, retrying...");
            continue;
        }

        if prompts::is_lgtm(&resp.output) {
            println!("[harness] LGTM — {url_display}");
            return Ok(());
        }

        prev_fixed = true;
        round += 1;
    }

    println!("[harness] Reached max rounds ({max_rounds}), PR status: {url_display}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        agent::{AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV},
        config::isolation::IsolationTier,
    };

    #[test]
    fn codex_review_spawn_env_uses_only_configured_spawn_controls() {
        let mut config = HarnessConfig::default();
        config.isolation.default_tier = IsolationTier::Container;
        config.isolation.network_allowlist = vec![
            "github.com".to_string(),
            " api.openai.com ".to_string(),
            String::new(),
        ];
        let process_env = HashMap::from([
            (
                "HARNESS_AGENT_CONTAINER_IMAGE".to_string(),
                "example/reviewer:sha256-test".to_string(),
            ),
            (
                "HARNESS_AGENT_EGRESS_PROXY".to_string(),
                "http://review-proxy.local:8080".to_string(),
            ),
            (
                "HARNESS_AGENT_EGRESS_PROXY_IMAGE".to_string(),
                "example/egress-proxy:sha256-test".to_string(),
            ),
            (
                "OPERATOR_API_KEY".to_string(),
                "operator-secret".to_string(),
            ),
        ]);

        let env_vars = codex_review_spawn_env_with(&config, |key| process_env.get(key).cloned());

        assert_eq!(
            env_vars.get(AGENT_ISOLATION_TIER_ENV).map(String::as_str),
            Some("container")
        );
        assert_eq!(
            env_vars
                .get(AGENT_NETWORK_ALLOWLIST_ENV)
                .map(String::as_str),
            Some("github.com,api.openai.com")
        );
        assert_eq!(
            env_vars
                .get("HARNESS_AGENT_CONTAINER_IMAGE")
                .map(String::as_str),
            Some("example/reviewer:sha256-test")
        );
        assert_eq!(
            env_vars
                .get("HARNESS_AGENT_EGRESS_PROXY_IMAGE")
                .map(String::as_str),
            Some("example/egress-proxy:sha256-test")
        );
        assert!(!env_vars.contains_key("HARNESS_AGENT_EGRESS_PROXY"));
        assert!(!env_vars.contains_key("OPERATOR_API_KEY"));
        assert!(!env_vars.values().any(|value| value == "operator-secret"));
    }

    #[tokio::test]
    async fn resolve_repo_slug_with_url_does_not_call_gh() -> anyhow::Result<()> {
        // When pr_url is Some, the slug is derived from the URL without
        // spawning any subprocess.
        let slug = resolve_repo_slug(42, Some("https://github.com/owner/myrepo/pull/42")).await?;
        assert_eq!(slug, "owner/myrepo");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_repo_slug_with_url_various_formats() -> anyhow::Result<()> {
        let cases = [
            ("https://github.com/org/repo/pull/1", "org/repo"),
            ("https://github.com/org/repo/pull/1/files", "org/repo"),
        ];
        for (url, expected) in cases {
            let slug = resolve_repo_slug(1, Some(url)).await?;
            assert_eq!(slug, expected, "url = {url}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolve_repo_slug_none_fails_without_gh() {
        // When pr_url is None, resolve_repo_slug calls `gh pr view`.  In a
        // test environment without a real gh context this should return an
        // Err rather than the literal "{owner}/{repo}" placeholder.
        //
        // We use a non-existent PR number to guarantee gh exits non-zero even
        // if gh is installed and authenticated.
        let result = resolve_repo_slug(u64::MAX, None).await;
        // Either gh is not installed (IoError) or it returned non-zero — both
        // map to Err.  The key assertion is that we do NOT get Ok("{owner}/{repo}").
        match result {
            Ok(slug) => {
                assert_ne!(
                    slug, "{owner}/{repo}",
                    "resolve_repo_slug must never return the literal placeholder"
                );
            }
            Err(_) => {
                // Expected path in CI / no-auth environments.
            }
        }
    }
}
