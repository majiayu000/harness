use harness_core::prompts;

pub(super) fn should_run_issue_triage(skip_triage: bool, has_existing_pr: bool) -> bool {
    !skip_triage && !has_existing_pr
}

pub(super) fn effective_agent_review_round_limit(
    request_max_rounds: Option<u32>,
    server_review_max_rounds: u32,
    triage_default_rounds: u32,
) -> u32 {
    request_max_rounds
        .or(Some(server_review_max_rounds))
        .unwrap_or(triage_default_rounds)
}

pub(super) fn effective_hosted_review_round_limit(
    request_max_rounds: Option<u32>,
    project_review_max_rounds: Option<u32>,
    server_review_max_rounds: u32,
    triage_default_rounds: u32,
) -> u32 {
    request_max_rounds
        .or(project_review_max_rounds)
        .or(Some(server_review_max_rounds))
        .unwrap_or(triage_default_rounds)
}

pub(super) fn local_review_pr_check_timeout_secs(
    wait_secs: u64,
    hosted_review_max_rounds: u32,
) -> u64 {
    wait_secs
        .max(60)
        .saturating_mul(u64::from(hosted_review_max_rounds.max(1)))
}

pub(super) fn initial_hosted_review_wait_secs(wait_secs: u64, review_wait_budget_secs: u64) -> u64 {
    if review_wait_budget_secs == 0 {
        wait_secs
    } else {
        wait_secs.min(review_wait_budget_secs)
    }
}

pub(super) fn review_repo_slug(pr_url: Option<&str>, detected_repo_slug: &str) -> String {
    pr_url
        .and_then(prompts::parse_github_pr_url)
        .map(|(owner, repo, _)| format!("{owner}/{repo}"))
        .unwrap_or_else(|| detected_repo_slug.to_string())
}
