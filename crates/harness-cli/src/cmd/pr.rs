use harness_agents::claude::ClaudeCodeAgent;
use harness_core::{prompts, AgentRequest, CodeAgent, HarnessConfig};
use std::path::PathBuf;
use tokio::time::{sleep, Duration};

fn create_agent(config: &HarnessConfig) -> ClaudeCodeAgent {
    ClaudeCodeAgent::new(
        config.agents.claude.cli_path.clone(),
        config.agents.claude.default_model.clone(),
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

    println!("[harness] Round 1 — 实现 issue #{issue} 并创建 PR");

    let req = AgentRequest {
        prompt: prompts::implement_from_issue(issue),
        project_root: project.clone(),
        ..Default::default()
    };

    let resp = agent.execute(req).await?;
    println!("{}", resp.output);

    let pr_url = prompts::parse_pr_url(&resp.output)
        .ok_or_else(|| anyhow::anyhow!("agent 输出中未找到 PR_URL=<url>"))?;
    let pr_number = prompts::extract_pr_number(&pr_url)
        .ok_or_else(|| anyhow::anyhow!("无法从 URL 解析 PR 编号: {pr_url}"))?;

    println!("[harness] PR #{pr_number} 已创建: {pr_url}");

    run_review_loop(
        &agent,
        &project,
        Some(issue),
        pr_number,
        &pr_url,
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

    println!("[harness] 进入 PR #{pr} 的 review loop");

    run_review_loop(&agent, &project, None, pr, "", wait, max_rounds).await
}

async fn run_review_loop(
    agent: &ClaudeCodeAgent,
    project: &PathBuf,
    issue: Option<u64>,
    pr: u64,
    pr_url: &str,
    wait: u64,
    max_rounds: u32,
) -> anyhow::Result<()> {
    let url_display = if pr_url.is_empty() {
        format!("PR #{pr}")
    } else {
        pr_url.to_string()
    };

    for round in 1..=max_rounds {
        println!("[harness] 等待 {wait}s，让 CI 和 review bot 运行...");
        sleep(Duration::from_secs(wait)).await;

        println!("[harness] Review 轮次 {round}/{max_rounds}，PR #{pr}");

        let req = AgentRequest {
            prompt: prompts::review_prompt(issue, pr),
            project_root: project.clone(),
            ..Default::default()
        };

        let resp = agent.execute(req).await?;
        println!("{}", resp.output);

        if prompts::is_lgtm(&resp.output) {
            println!("[harness] LGTM — {url_display}");
            return Ok(());
        }
    }

    println!("[harness] 已达最大轮次 ({max_rounds})，PR 状态: {url_display}");
    Ok(())
}
