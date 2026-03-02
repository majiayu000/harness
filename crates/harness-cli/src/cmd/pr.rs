use harness_agents::claude::ClaudeCodeAgent;
use harness_core::{AgentRequest, CodeAgent, HarnessConfig};
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
    let prompt = format!(
        "读 GitHub issue #{issue}，理解需求，在当前项目中实现代码，\
         运行 cargo check 和 cargo test，创建功能分支，commit，push，\
         用 gh pr create 创建 PR。\
         完成后在输出的最后一行单独输出 PR_URL=<完整PR URL>",
        issue = issue,
    );

    let req = AgentRequest {
        prompt,
        project_root: project.clone(),
        ..Default::default()
    };

    let resp = agent.execute(req).await?;
    println!("{}", resp.output);

    let pr_url = parse_pr_url(&resp.output)
        .ok_or_else(|| anyhow::anyhow!("agent 输出中未找到 PR_URL=<url>"))?;
    let pr_number = extract_pr_number(&pr_url)
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

        let context = match issue {
            Some(n) => format!("你之前为 issue #{n} 创建了 PR #{pr}。\n"),
            None => format!("检查 PR #{pr}。\n"),
        };

        let prompt = format!(
            "{context}\
             现在用 gh pr checks 检查 CI 状态，\
             用 gh pr view 和 gh api 读取 review comments。\
             如果 CI 通过且没有需要处理的 review comments，在最后一行单独输出 LGTM。\
             否则根据反馈修复代码，commit，push，在最后一行单独输出 FIXED。",
            context = context,
        );

        let req = AgentRequest {
            prompt,
            project_root: project.clone(),
            ..Default::default()
        };

        let resp = agent.execute(req).await?;
        println!("{}", resp.output);

        if resp.output.trim().ends_with("LGTM") {
            println!("[harness] LGTM — {url_display}");
            return Ok(());
        }
        // FIXED or other → continue to next round
    }

    println!("[harness] 已达最大轮次 ({max_rounds})，PR 状态: {url_display}");
    Ok(())
}

fn parse_pr_url(output: &str) -> Option<String> {
    for line in output.lines().rev() {
        let line = line.trim();
        if let Some(url) = line.strip_prefix("PR_URL=") {
            return Some(url.trim().to_string());
        }
    }
    None
}

fn extract_pr_number(url: &str) -> Option<u64> {
    url.split('/').last()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pr_url() {
        let output = "Some agent output\nPR_URL=https://github.com/owner/repo/pull/42";
        assert_eq!(
            parse_pr_url(output),
            Some("https://github.com/owner/repo/pull/42".to_string())
        );
    }

    #[test]
    fn test_parse_pr_url_not_found() {
        let output = "No PR URL in this output";
        assert_eq!(parse_pr_url(output), None);
    }

    #[test]
    fn test_extract_pr_number() {
        assert_eq!(
            extract_pr_number("https://github.com/owner/repo/pull/42"),
            Some(42)
        );
    }

    #[test]
    fn test_extract_pr_number_invalid() {
        assert_eq!(extract_pr_number("https://github.com/owner/repo/pull/"), None);
    }
}
