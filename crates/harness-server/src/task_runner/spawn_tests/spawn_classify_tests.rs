use super::super::*;
use crate::task_runner::request::SystemTaskInput;

#[test]
fn parse_pr_url_standard() {
    let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/42") else {
        panic!("expected Some for standard GitHub PR URL");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 42);
}

#[test]
fn parse_pr_url_with_fragment() {
    let Some((owner, repo, number)) =
        parse_pr_url("https://github.com/acme/myrepo/pull/99#issuecomment-123")
    else {
        panic!("expected Some for PR URL with fragment");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 99);
}

#[test]
fn parse_pr_url_trailing_slash() {
    let Some((owner, repo, number)) = parse_pr_url("https://github.com/acme/myrepo/pull/7/") else {
        panic!("expected Some for PR URL with trailing slash");
    };
    assert_eq!(owner, "acme");
    assert_eq!(repo, "myrepo");
    assert_eq!(number, 7);
}

#[test]
fn parse_pr_url_invalid_returns_none() {
    assert!(parse_pr_url("https://github.com/acme/myrepo").is_none());
    assert!(parse_pr_url("not-a-url").is_none());
    assert!(parse_pr_url("https://github.com/acme/myrepo/issues/1").is_none());
}

// --- planning gate ---

#[test]
fn short_prompt_does_not_require_plan() {
    let prompt = "Fix the typo in README.md";
    assert!(!prompt_requires_plan(prompt));
}

#[test]
fn long_prompt_over_200_words_requires_plan() {
    let words: Vec<&str> = std::iter::repeat_n("word", 201).collect();
    let prompt = words.join(" ");
    assert!(prompt_requires_plan(&prompt));
}

#[test]
fn prompt_with_three_file_paths_requires_plan() {
    let prompt = "Update src/foo.rs and crates/bar/src/lib.rs and crates/baz/src/main.rs to fix X";
    assert!(prompt_requires_plan(prompt));
}

#[test]
fn prompt_with_two_file_paths_does_not_require_plan() {
    let prompt = "Update src/foo.rs and crates/bar/src/lib.rs to fix X";
    assert!(!prompt_requires_plan(prompt));
}

#[test]
fn xml_closing_tags_are_not_counted_as_file_paths() {
    // `wrap_external_data` wraps content in <external_data>...</external_data>.
    // Three `</external_data>` closing tags contain '/' but must NOT trigger
    // the planning gate — they are markup, not file paths.
    let prompt = "GC applied files:\n\
                  <external_data>test-guard.sh</external_data>\n\
                  Rationale:\n<external_data>test</external_data>\n\
                  Validation:\n<external_data>test</external_data>";
    assert!(!prompt_requires_plan(prompt));
}

// --- TaskKind::classify cases ---

#[test]
fn request_task_kind_trusts_only_internal_system_input() {
    let spoofed = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        ..Default::default()
    };
    assert_eq!(spoofed.task_kind(), TaskKind::Prompt);

    let trusted = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        system_input: Some(SystemTaskInput::PeriodicReview {
            prompt: "review this".to_string(),
        }),
        ..Default::default()
    };
    assert_eq!(trusted.task_kind(), TaskKind::Review);
}

#[test]
fn system_input_for_request_clones_only_explicit_internal_metadata() {
    let spoofed = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        ..Default::default()
    };
    assert_eq!(spoofed.system_input.clone(), None);

    let trusted = CreateTaskRequest {
        prompt: Some("review this".to_string()),
        source: Some("periodic_review".to_string()),
        system_input: Some(SystemTaskInput::PeriodicReview {
            prompt: "review this".to_string(),
        }),
        ..Default::default()
    };
    assert_eq!(
        trusted.system_input.clone(),
        Some(SystemTaskInput::PeriodicReview {
            prompt: "review this".to_string(),
        })
    );
}

// ── GC trigger demotion tests (issue #969) ────────────────────────────

#[test]
fn is_issue_pr_task_classifies_external_ids() {
    assert!(
        is_issue_pr_task(Some("issue:42")),
        "issue-keyed task must be classified as issue/PR"
    );
    assert!(
        is_issue_pr_task(Some("pr:123")),
        "pr-keyed task must be classified as issue/PR"
    );
    assert!(
        !is_issue_pr_task(Some("7f3a8b2c-1234-5678-abcd-ef0123456789")),
        "UUID task must not be classified as issue/PR"
    );
    assert!(
        !is_issue_pr_task(None),
        "prompt-only task (no external_id) must not be classified as issue/PR"
    );
}
