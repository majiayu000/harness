use super::*;

#[test]
fn exponential_backoff_three_consecutive_retries() {
    let base_ms: u64 = 10_000;
    let max_ms: u64 = 300_000;

    // Attempt 1: base_ms * 2^0 = 10_000 ms (10 s)
    assert_eq!(compute_backoff_ms(base_ms, max_ms, 1), 10_000);
    // Attempt 2: base_ms * 2^1 = 20_000 ms (20 s)
    assert_eq!(compute_backoff_ms(base_ms, max_ms, 2), 20_000);
    // Attempt 3: base_ms * 2^2 = 40_000 ms (40 s)
    assert_eq!(compute_backoff_ms(base_ms, max_ms, 3), 40_000);
}

#[test]
fn exponential_backoff_capped_at_max() {
    let base_ms: u64 = 10_000;
    let max_ms: u64 = 300_000;

    // Attempt 6: 10_000 * 2^5 = 320_000 — should be capped at 300_000
    assert_eq!(compute_backoff_ms(base_ms, max_ms, 6), 300_000);
}

#[test]
fn parse_implementation_outcome_prefers_plan_issue() {
    let output = "PLAN_ISSUE=Plan missed rollback path\nPR_URL=https://github.com/o/r/pull/123";
    let parsed = parse_implementation_outcome(output).expect("plan issue should parse");
    assert_eq!(
        parsed,
        ImplementationOutcome::PlanIssue("Plan missed rollback path".to_string())
    );
}

#[test]
fn parse_implementation_outcome_extracts_pr_when_no_plan_issue() {
    let output = "Done.\nPR_URL=https://github.com/majiayu000/harness/pull/42";
    let parsed = parse_implementation_outcome(output).expect("pr output should parse");
    assert_eq!(
        parsed,
        ImplementationOutcome::ParsedPr {
            pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
            pr_num: Some(42),
            created_issue_num: None,
            pushed_commit: None,
            review_prep: None,
        }
    );
}

#[test]
fn parse_implementation_outcome_extracts_rebase_pushed() {
    let output = "Prepared.\nREBASE_PUSHED\nPR_URL=https://github.com/majiayu000/harness/pull/42";
    let parsed = parse_implementation_outcome(output).expect("rebase prep should parse");
    assert_eq!(
        parsed,
        ImplementationOutcome::ParsedPr {
            pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
            pr_num: Some(42),
            created_issue_num: None,
            pushed_commit: None,
            review_prep: Some(prompts::PrReviewPrepOutcome::RebasePushed),
        }
    );
}

#[test]
fn parse_implementation_outcome_extracts_rebase_skipped() {
    let output = "Prepared.\nREBASE_SKIPPED\nPR_URL=https://github.com/majiayu000/harness/pull/42";
    let parsed = parse_implementation_outcome(output).expect("rebase prep should parse");
    assert_eq!(
        parsed,
        ImplementationOutcome::ParsedPr {
            pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
            pr_num: Some(42),
            created_issue_num: None,
            pushed_commit: None,
            review_prep: Some(prompts::PrReviewPrepOutcome::RebaseSkipped),
        }
    );
}

#[test]
fn parse_implementation_outcome_extracts_rebase_conflict() {
    let output = "Prepared.\nREBASE_CONFLICT paths=src/lib.rs,src/main.rs\nPR_URL=https://github.com/majiayu000/harness/pull/42";
    let parsed = parse_implementation_outcome(output).expect("rebase prep should parse");
    assert_eq!(
        parsed,
        ImplementationOutcome::ParsedPr {
            pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
            pr_num: Some(42),
            created_issue_num: None,
            pushed_commit: None,
            review_prep: Some(prompts::PrReviewPrepOutcome::RebaseConflict {
                paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            }),
        }
    );
}

#[test]
fn parse_implementation_outcome_extracts_pushed_commit_flag() {
    let output = "PR_URL=https://github.com/majiayu000/harness/pull/42\nPUSHED_COMMIT=true\nFIXED";
    let parsed = parse_implementation_outcome(output).expect("pushed flag should parse");
    assert_eq!(
        parsed,
        ImplementationOutcome::ParsedPr {
            pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
            pr_num: Some(42),
            created_issue_num: None,
            pushed_commit: Some(true),
            review_prep: None,
        }
    );
}

#[test]
fn parse_implementation_outcome_rejects_malformed_pushed_commit_flag() {
    let output = "PR_URL=https://github.com/majiayu000/harness/pull/42\nPUSHED_COMMIT=maybe\nFIXED";
    assert_eq!(
        parse_implementation_outcome(output),
        Err("invalid PUSHED_COMMIT value `maybe`; expected true or false".to_string())
    );
}

#[test]
fn resolve_pushed_commit_flag_requires_marker_for_direct_pr_checks() {
    assert_eq!(resolve_pushed_commit_flag(true, Some(true), None), Ok(true));
    assert_eq!(resolve_pushed_commit_flag(false, None, None), Ok(false));
    assert_eq!(
            resolve_pushed_commit_flag(true, None, None),
            Err(
                "missing required PUSHED_COMMIT marker in PR-check output; refusing to skip freshness gate"
                    .to_string()
            )
        );
}

#[test]
fn resolve_pushed_commit_flag_uses_rebase_prep_for_direct_pr_checks() {
    assert_eq!(
        resolve_pushed_commit_flag(
            true,
            None,
            Some(&prompts::PrReviewPrepOutcome::RebasePushed)
        ),
        Ok(true)
    );
    assert_eq!(
        resolve_pushed_commit_flag(
            true,
            None,
            Some(&prompts::PrReviewPrepOutcome::RebaseSkipped)
        ),
        Ok(false)
    );
}

#[test]
fn worktree_collision_sentinel_detected() {
    let collision_output =
            "Pushed. Now clean up the worktree (it's managed by another harness session so I'll skip removing it):\nPR_URL=https://github.com/owner/repo/pull/796";
    assert!(
        contains_worktree_collision_sentinel(collision_output),
        "sentinel should be detected in collision output"
    );
}

#[test]
fn worktree_collision_sentinel_absent_in_normal_output() {
    let normal_output = "Done.\nPR_URL=https://github.com/owner/repo/pull/42";
    assert!(
        !contains_worktree_collision_sentinel(normal_output),
        "sentinel should not be detected in normal output"
    );
}

#[test]
fn constitution_present_when_enabled() {
    let result = prepend_constitution("Do the task.".to_string(), true);
    assert!(result.contains("GP-01"));
    assert!(result.contains("GP-02"));
    assert!(result.contains("GP-03"));
    assert!(result.contains("GP-04"));
    assert!(result.contains("GP-05"));
    assert!(result.ends_with("Do the task."));
}

#[test]
fn constitution_absent_when_disabled() {
    let result = prepend_constitution("Do the task.".to_string(), false);
    assert_eq!(result, "Do the task.");
    assert!(!result.contains("GP-01"));
}
