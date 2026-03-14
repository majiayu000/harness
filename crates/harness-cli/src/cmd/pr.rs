use harness_agents::claude::ClaudeCodeAgent;
use harness_core::{prompts, AgentRequest, CodeAgent, HarnessConfig};
use std::path::PathBuf;
use tokio::time::{sleep, Duration};

fn create_agent(config: &HarnessConfig) -> ClaudeCodeAgent {
    ClaudeCodeAgent::new(
        config.agents.claude.cli_path.clone(),
        config.agents.claude.default_model.clone(),
        config.agents.sandbox_mode,
    )
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
        prompt: prompts::implement_from_issue(issue, None),
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
