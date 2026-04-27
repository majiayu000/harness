use harness_agents::claude::ClaudeCodeAgent;
use harness_core::{agent::AgentRequest, agent::CodeAgent, config::HarnessConfig, prompts};
use std::path::PathBuf;
use tokio::time::{sleep, Duration};

fn create_agent(config: &HarnessConfig) -> ClaudeCodeAgent {
    ClaudeCodeAgent::new(
        config.agents.claude.cli_path.clone(),
        config.agents.claude.default_model.clone(),
        config.agents.sandbox_mode,
    )
    .with_no_session_persistence_probe()
    .with_stream_timeout(config.agents.stream_timeout_secs)
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

    let req = AgentRequest {
        prompt: prompts::implement_from_issue(issue, None, None).to_prompt_string(),
        project_root: project.clone(),
        ..Default::default()
    };

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
        Some(issue),
        pr_number,
        Some(&pr_url),
        wait,
        max_rounds,
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

    run_review_loop(&agent, &project, None, pr, None, wait, max_rounds).await
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

async fn run_review_loop(
    agent: &impl CodeAgent,
    project: &PathBuf,
    issue: Option<u64>,
    pr: u64,
    pr_url: Option<&str>,
    wait: u64,
    max_rounds: u32,
) -> anyhow::Result<()> {
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
                "/gemini review",
                "gemini-code-assist[bot]",
                &repo,
                false,
            ),
            project_root: project.clone(),
            ..Default::default()
        };

        let resp = agent.execute(req).await?;
        println!("{}", resp.output);

        if prompts::is_waiting(&resp.output) {
            println!("[harness] Gemini hasn't re-reviewed yet, retrying...");
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
