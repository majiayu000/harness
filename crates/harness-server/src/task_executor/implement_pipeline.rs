use crate::task_runner::CreateTaskRequest;
use harness_core::prompts;
use harness_core::types::{ContextItem, TurnFailure, TurnFailureKind};
use tokio::time::Duration;

mod flow;
mod outcome;
mod turn;

pub(crate) use flow::run_implement_phase;

/// Compute exponential backoff: `min(base_ms * 2^(attempt-1), max_ms)`.
///
/// - Attempt 1: `base_ms`
/// - Attempt 2: `base_ms * 2`
/// - Attempt 3: `base_ms * 4`
/// - …capped at `max_ms`
pub(crate) fn compute_backoff_ms(base_ms: u64, max_ms: u64, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(63);
    base_ms.saturating_mul(1u64 << shift).min(max_ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImplementationOutcome {
    PlanIssue(String),
    ParsedPr {
        pr_url: Option<String>,
        pr_num: Option<u64>,
        created_issue_num: Option<u64>,
        pushed_commit: Option<bool>,
        review_prep: Option<prompts::PrReviewPrepOutcome>,
    },
}

pub(crate) fn parse_implementation_outcome(output: &str) -> Result<ImplementationOutcome, String> {
    if let Some(desc) = prompts::parse_plan_issue(output) {
        return Ok(ImplementationOutcome::PlanIssue(desc));
    }
    let pr_url = prompts::parse_pr_url(output);
    let pr_num = pr_url.as_deref().and_then(prompts::extract_pr_number);
    let created_issue_num = prompts::parse_created_issue_number(output);
    let pushed_commit = prompts::parse_pushed_commit(output)?;
    let review_prep = prompts::parse_pr_review_prep_outcome(output);
    Ok(ImplementationOutcome::ParsedPr {
        pr_url,
        pr_num,
        created_issue_num,
        pushed_commit,
        review_prep,
    })
}

fn resolve_pushed_commit_flag(
    is_direct_pr_check: bool,
    pushed_commit: Option<bool>,
    review_prep: Option<&prompts::PrReviewPrepOutcome>,
) -> Result<bool, String> {
    match (pushed_commit, review_prep) {
        (Some(pushed_commit), _) => Ok(pushed_commit),
        (None, Some(prompts::PrReviewPrepOutcome::RebasePushed)) => Ok(true),
        (None, Some(prompts::PrReviewPrepOutcome::RebaseSkipped))
        | (None, Some(prompts::PrReviewPrepOutcome::RebaseConflict { .. })) => Ok(false),
        (None, None) if !is_direct_pr_check => Ok(false),
        (None, None) => Err(
            "missing required PUSHED_COMMIT marker in PR-check output; refusing to skip freshness gate"
                .to_string(),
        ),
    }
}

/// Returns true if the agent output contains the sentinel string that indicates
/// the agent is operating inside a stale worktree owned by another harness session.
pub(crate) fn contains_worktree_collision_sentinel(output: &str) -> bool {
    output.contains("managed by another harness session")
}

pub(crate) fn prepend_constitution(prompt: String, enabled: bool) -> String {
    const CONSTITUTION: &str = include_str!("../../../../config/constitution.md");
    if enabled {
        format!("{CONSTITUTION}\n\n{prompt}")
    } else {
        prompt
    }
}

/// Returns true when the task type requires the agent to emit a `PR_URL=…` line.
/// Issue-backed tasks and `pr:N` review tasks must produce a PR URL; generic
/// prompt-only implementation tasks may complete without creating one.
pub(crate) fn task_needs_pr_url(req: &CreateTaskRequest) -> bool {
    req.task_kind().requires_pr_url()
}

fn implementation_failure_result(failure: &TurnFailure) -> &'static str {
    match failure.kind {
        TurnFailureKind::Timeout => "timeout",
        TurnFailureKind::Quota => "quota_exhausted",
        TurnFailureKind::Billing => "billing_failed",
        TurnFailureKind::Upstream => "upstream_failure",
        TurnFailureKind::LocalProcess => "local_process_failure",
        TurnFailureKind::Protocol => "protocol_failure",
        TurnFailureKind::Unknown => "failed",
    }
}

/// Outcome of the implement phase.
#[derive(Debug)]
pub(crate) enum ImplementOutcome {
    /// Task is fully complete (success or failure already persisted) — caller returns Ok(()).
    Done,
    /// Implementation reported that the current plan is incomplete and the
    /// caller should choose between replan or force-execute retry.
    Replan {
        issue: u64,
        plan_issue: String,
        prior_plan: Option<String>,
    },
    /// Implementation succeeded — proceed to conflict resolution and review.
    Proceed {
        pr_url: Option<String>,
        pr_num: u64,
        implementation_pushed_commit: bool,
        review_prep: Option<prompts::PrReviewPrepOutcome>,
        context_items: Vec<ContextItem>,
        #[allow(dead_code)]
        turn_timeout: Duration,
        #[allow(dead_code)]
        initial_allowed_tools: Option<Vec<String>>,
    },
}

#[cfg(test)]
#[path = "implement_pipeline/tests.rs"]
mod tests;
