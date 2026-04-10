use super::*;
#[test]
fn test_review_synthesis_prompt_with_custom_agents() {
    let p = review_synthesis_prompt_with_agents("Codex", "left", "Claude", "right");
    assert!(p.contains("Codex and Claude"));
    assert!(p.contains("## Codex's Review"));
    assert!(p.contains("## Claude's Review"));
}

#[test]
fn test_implement_from_prompt() {
    let p = implement_from_prompt("fix the bug", None);
    assert!(p.contains("fix the bug"));
    assert!(p.contains("PR_URL="));
}

#[test]
fn test_review_prompt_with_issue() {
    let p = review_prompt(
        Some(5),
        10,
        2,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("issue #5"));
    assert!(p.contains("PR #10"));
    assert!(p.contains("medium")); // round 2 includes medium
}

#[test]
fn test_review_prompt_without_issue() {
    let p = review_prompt(
        None,
        10,
        2,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("PR #10"));
    assert!(!p.contains("issue #")); // no issue reference when None
}

#[test]
fn test_review_prompt_late_round_skips_medium() {
    let p = review_prompt(
        None,
        10,
        3,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("Skip medium"));
}

#[test]
fn test_review_prompt_uses_configured_review_bot_command() {
    let p = review_prompt(
        None,
        10,
        2,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("/gemini review"));
    let p = review_prompt(
        None,
        10,
        2,
        false,
        "/reviewbot run",
        "reviewbot[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("/reviewbot run"));
    assert!(!p.contains("/gemini"));
}

#[test]
fn test_review_prompt_always_triggers_gemini_review() {
    let p = review_prompt(
        None,
        10,
        2,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("/gemini review"));
    let p = review_prompt(
        None,
        10,
        4,
        true,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("/gemini review"));
}

#[test]
fn test_review_prompt_prev_fixed_requires_freshness_check() {
    let p = review_prompt(
        None,
        10,
        3,
        true,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("WAITING"));
    assert!(p.contains("latest review was submitted BEFORE the latest commit"));
    // Without prev_fixed, no freshness check section
    let p = review_prompt(
        None,
        10,
        3,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(!p.contains("latest review was submitted BEFORE the latest commit"));
}

#[test]
fn test_review_prompt_constraints() {
    let p = review_prompt(
        None,
        10,
        2,
        false,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("NEVER downgrade dependency"));
}

#[test]
fn test_review_prompt_freshness_check_filters_by_reviewer_login() {
    let p = review_prompt(
        None,
        10,
        2,
        true,
        "/gemini review",
        "gemini-code-assist[bot]",
        "owner/repo",
        false,
    );
    assert!(p.contains("gemini-code-assist[bot]"));
    assert!(p.contains(".user.login"));
    // A different reviewer's login must appear in its own prompt
    let p2 = review_prompt(
        None,
        10,
        2,
        true,
        "/reviewbot run",
        "acme-bot[bot]",
        "owner/repo",
        false,
    );
    assert!(p2.contains("acme-bot[bot]"));
    assert!(!p2.contains("gemini-code-assist[bot]"));
}

#[test]
fn test_check_existing_pr_shell_quoting() {
    // A command containing a single quote must not break single-quoting
    let p = check_existing_pr(5, "it's a test", "owner/repo", "bot[bot]", false);
    assert!(
        p.contains(r"'it'\''s a test'"),
        "single quote must be escaped"
    );
}

#[test]
fn test_review_prompt_shell_quoting() {
    let p = review_prompt(
        None,
        5,
        2,
        false,
        "it's a test",
        "bot[bot]",
        "owner/repo",
        false,
    );
    assert!(
        p.contains(r"'it'\''s a test'"),
        "single quote must be escaped"
    );
}
