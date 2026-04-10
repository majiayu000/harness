use super::*;
#[test]
fn test_sibling_task_context_empty() {
    assert_eq!(sibling_task_context(&[]), String::new());
}

#[test]
fn test_sibling_task_context_with_issue_siblings() {
    let siblings = vec![
        SiblingTask {
            issue: Some(77),
            description: "fix Mistral transform_request unwrap".to_string(),
        },
        SiblingTask {
            issue: Some(78),
            description: "fix Vertex AI unwrap".to_string(),
        },
    ];
    let ctx = sibling_task_context(&siblings);
    assert!(ctx.contains("#77: fix Mistral transform_request unwrap"));
    assert!(ctx.contains("#78: fix Vertex AI unwrap"));
    assert!(ctx.contains("OTHER agents"));
    assert!(ctx.contains("Only modify files directly needed for YOUR assigned task."));
}

#[test]
fn test_sibling_task_context_without_issue_number() {
    let siblings = vec![SiblingTask {
        issue: None,
        description: "refactor auth middleware".to_string(),
    }];
    let ctx = sibling_task_context(&siblings);
    assert!(ctx.contains("- refactor auth middleware"));
    assert!(!ctx.contains('#'));
}

#[test]
fn build_available_skills_listing_empty_returns_empty_string() {
    assert!(build_available_skills_listing(std::iter::empty::<(&str, &str)>()).is_empty());
}

#[test]
fn build_available_skills_listing_formats_all_entries() {
    let skills = [
        ("review", "Code review tool"),
        ("deploy", "Deploy to production"),
    ];
    let result = build_available_skills_listing(skills.iter().copied());
    assert!(result.contains("## Available Skills"));
    assert!(result.contains("**review**"));
    assert!(result.contains("Code review tool"));
    assert!(result.contains("**deploy**"));
    assert!(result.contains("Deploy to production"));
}

#[test]
fn build_available_skills_listing_starts_with_double_newline() {
    let skills = [("a", "desc")];
    let result = build_available_skills_listing(skills.iter().copied());
    assert!(
        result.starts_with("\n\n"),
        "section must start with two newlines"
    );
}

#[test]
fn build_matched_skills_section_empty_returns_empty_string() {
    assert!(build_matched_skills_section(std::iter::empty::<(&str, &str)>()).is_empty());
}

#[test]
fn build_matched_skills_section_formats_name_and_content() {
    let skills = [("review", "# Review\nReview code carefully.")];
    let result = build_matched_skills_section(skills.iter().copied());
    assert!(result.contains("## Relevant Skills"));
    assert!(result.contains("### review"));
    assert!(result.contains("Review code carefully."));
}

#[test]
fn build_matched_skills_section_starts_with_double_newline() {
    let skills = [("a", "content")];
    let result = build_matched_skills_section(skills.iter().copied());
    assert!(
        result.starts_with("\n\n"),
        "section must start with two newlines"
    );
}

// --- Triage / Plan phase tests ---
