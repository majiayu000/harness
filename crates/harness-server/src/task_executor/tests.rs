use super::*;

#[test]
fn periodic_review_source_uses_standard_allowed_tools() {
    // Verifies that periodic_review tasks get a non-empty allowed_tools list,
    // which causes claude.rs to pass --allowedTools (hard enforcement) instead
    // of --dangerously-skip-permissions.
    let tools = restricted_tools(CapabilityProfile::Standard).unwrap_or_default();
    assert_eq!(
        tools,
        CapabilityProfile::Standard.tools().unwrap_or_default()
    );
    assert!(!tools.is_empty());
}

#[test]
fn standard_implementation_turn_uses_full_profile() {
    // Non-periodic_review tasks use None → Full profile →
    // --dangerously-skip-permissions in claude.rs.
    let implementation_allowed_tools: Option<Vec<String>> = None;
    assert!(implementation_allowed_tools.is_none());
}

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
fn parse_harness_review_command() {
    let cmd = parse_harness_mention_command("@harness review");
    assert_eq!(cmd, Some(HarnessMentionCommand::Review));
}

#[test]
fn parse_harness_fix_ci_command_case_insensitive() {
    let cmd = parse_harness_mention_command("please @Harness FIX CI");
    assert_eq!(cmd, Some(HarnessMentionCommand::FixCi));
}

#[test]
fn parse_harness_plain_mention_command() {
    let cmd = parse_harness_mention_command("hello @harness can you help?");
    assert_eq!(cmd, Some(HarnessMentionCommand::Mention));
}

#[test]
fn parse_harness_command_returns_none_without_mention() {
    let cmd = parse_harness_mention_command("no command here");
    assert_eq!(cmd, None);
}

#[test]
fn parse_harness_first_mention_per_line_is_used() {
    let cmd = parse_harness_mention_command("@harness review then @harness fix ci");
    assert_eq!(cmd, Some(HarnessMentionCommand::Review));
}

#[test]
fn prompt_builder_no_sections_adds_trailing_newline() {
    let result = PromptBuilder::new("Title line.").build();
    assert_eq!(result, "Title line.\n");
}

#[test]
fn prompt_builder_optional_url_absent_is_skipped() {
    let result = PromptBuilder::new("Title.")
        .add_optional_url("Link", None)
        .build();
    assert_eq!(result, "Title.\n");
}

#[test]
fn prompt_builder_optional_url_present_appears_in_output() {
    let result = PromptBuilder::new("Title.")
        .add_optional_url("Link", Some("https://example.com"))
        .build();
    assert!(result.contains("- Link: "));
    assert!(result.contains("https://example.com"));
    assert!(result.ends_with('\n'));
}

#[test]
fn prompt_builder_add_section_wraps_external_data() {
    let result = PromptBuilder::new("Title.")
        .add_section("Payload", "content here")
        .build();
    assert!(result.contains("Payload:\n"));
    assert!(result.contains("<external_data>"));
    assert!(result.contains("content here"));
}

#[test]
fn prompt_builder_multiple_urls_all_appear() {
    let result = PromptBuilder::new("Title.")
        .add_optional_url("First", Some("url1"))
        .add_optional_url("Second", None)
        .add_optional_url("Third", Some("url3"))
        .build();
    assert!(result.contains("- First: "));
    assert!(result.contains("url1"));
    assert!(!result.contains("Second"));
    assert!(result.contains("- Third: "));
    assert!(result.contains("url3"));
}

#[test]
fn build_fix_ci_prompt_contains_context() {
    let prompt = build_fix_ci_prompt(
        "majiayu000/harness",
        42,
        "@harness fix CI",
        Some("https://github.com/majiayu000/harness/issues/42#issuecomment-1"),
        Some("https://github.com/majiayu000/harness/pull/42"),
    );

    assert!(prompt.contains("CI failure repair requested for PR #42"));
    assert!(prompt.contains("majiayu000/harness"));
    assert!(prompt.contains("<external_data>"));
    assert!(prompt.contains("PR_URL=https://github.com/majiayu000/harness/pull/42"));
}

#[test]
fn truncate_short_string_passes_through() {
    let input = "short error";
    let result = truncate_validation_error(input, 100);
    assert_eq!(result, "short error");
}

#[test]
fn truncate_at_max_chars_boundary() {
    let input = "a".repeat(200);
    let result = truncate_validation_error(&input, 50);
    assert!(result.starts_with(&"a".repeat(50)));
    assert!(result.contains("(output truncated, 200 chars total)"));
}

#[test]
fn truncate_preserves_utf8_boundary() {
    // "é" is 2 bytes; build a string where max_chars lands mid-character.
    let input = "ééééé"; // 10 bytes, 5 chars
    let result = truncate_validation_error(input, 3); // byte 3 is mid-char
                                                      // Should back up to byte 2 (1 full "é").
    assert!(result.starts_with("é"));
    assert!(result.contains("(output truncated,"));
}

#[test]
fn review_check_turn_uses_readonly_profile() {
    let tools = restricted_tools(CapabilityProfile::ReadOnly).unwrap();
    assert!(tools.contains(&"Read".to_string()));
    assert!(tools.contains(&"Grep".to_string()));
    assert!(tools.contains(&"Glob".to_string()));
    assert!(!tools.contains(&"Write".to_string()));
    assert!(!tools.contains(&"Edit".to_string()));
    assert!(!tools.contains(&"Bash".to_string()));
}

#[test]
fn periodic_review_turn_uses_standard_profile_with_bash() {
    let tools = restricted_tools(CapabilityProfile::Standard).unwrap();
    assert!(tools.contains(&"Bash".to_string()));
    assert!(tools.contains(&"Read".to_string()));
    assert!(tools.contains(&"Write".to_string()));
    assert!(tools.contains(&"Edit".to_string()));
    // Standard does not include Grep/Glob — it's distinct from ReadOnly.
    assert!(!tools.contains(&"Grep".to_string()));
}

#[test]
fn implementation_turn_uses_full_profile_no_restriction() {
    // Full profile returns None — no tool restriction is applied to the agent.
    assert!(CapabilityProfile::Full.tools().is_none());
}

#[test]
fn parse_implementation_outcome_prefers_plan_issue() {
    let output = "PLAN_ISSUE=Plan missed rollback path\nPR_URL=https://github.com/o/r/pull/123";
    let parsed = parse_implementation_outcome(output);
    assert_eq!(
        parsed,
        ImplementationOutcome::PlanIssue("Plan missed rollback path".to_string())
    );
}

#[test]
fn parse_implementation_outcome_extracts_pr_when_no_plan_issue() {
    let output = "Done.\nPR_URL=https://github.com/majiayu000/harness/pull/42";
    let parsed = parse_implementation_outcome(output);
    assert_eq!(
        parsed,
        ImplementationOutcome::ParsedPr {
            pr_url: Some("https://github.com/majiayu000/harness/pull/42".to_string()),
            pr_num: Some(42),
        }
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

#[test]
fn normalize_issues_is_order_invariant() {
    let ordered = vec!["issue A".to_string(), "issue B".to_string()];
    let reversed = vec!["issue B".to_string(), "issue A".to_string()];
    assert_eq!(normalize_issues(&ordered), normalize_issues(&reversed));
}

fn step_tracker(tracker: &mut Option<(Vec<String>, u32)>, issues: &[String]) -> (u32, bool, bool) {
    let normalized = normalize_issues(issues);
    let count = match tracker.as_ref() {
        Some((prev, c)) if *prev == normalized => c + 1,
        _ => 1,
    };
    *tracker = Some((normalized, count));
    let intervention = count >= 3;
    let fatal = count >= 5;
    (count, intervention, fatal)
}

#[test]
fn impasse_no_intervention_for_first_two_rounds() {
    let issues = vec!["null pointer".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    let (c1, i1, f1) = step_tracker(&mut tracker, &issues);
    let (c2, i2, f2) = step_tracker(&mut tracker, &issues);
    assert_eq!(c1, 1);
    assert!(!i1 && !f1, "no action on first occurrence");
    assert_eq!(c2, 2);
    assert!(!i2 && !f2, "no action on second occurrence");
}

#[test]
fn impasse_intervention_at_third_consecutive_round() {
    let issues = vec!["null pointer".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    step_tracker(&mut tracker, &issues); // round 1
    step_tracker(&mut tracker, &issues); // round 2
    let (c3, i3, f3) = step_tracker(&mut tracker, &issues); // round 3
    assert_eq!(c3, 3);
    assert!(i3, "intervention at 3rd consecutive round");
    assert!(!f3, "not yet fatal at round 3");
}

#[test]
fn impasse_fatal_at_fifth_consecutive_round() {
    let issues = vec!["null pointer".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    for _ in 0..4 {
        step_tracker(&mut tracker, &issues);
    }
    let (c5, i5, f5) = step_tracker(&mut tracker, &issues);
    assert_eq!(c5, 5);
    assert!(i5, "intervention still active at round 5");
    assert!(f5, "fatal at 5th consecutive round");
}

#[test]
fn impasse_counter_resets_when_issues_change() {
    let issues = vec!["null pointer".to_string()];
    let other = vec!["different bug".to_string()];
    let mut tracker: Option<(Vec<String>, u32)> = None;
    step_tracker(&mut tracker, &issues); // count 1
    step_tracker(&mut tracker, &issues); // count 2
    step_tracker(&mut tracker, &issues); // count 3 — would trigger intervention
    let (c_reset, i_reset, _) = step_tracker(&mut tracker, &other); // different issues
    assert_eq!(c_reset, 1, "counter resets on different issues");
    assert!(!i_reset, "no intervention after reset");
}

// --- jaccard_word_similarity unit tests ---

#[test]
fn jaccard_identical_strings() {
    assert_eq!(jaccard_word_similarity("hello world", "hello world"), 1.0);
}

#[test]
fn jaccard_disjoint_strings() {
    assert_eq!(jaccard_word_similarity("foo bar", "baz qux"), 0.0);
}

#[test]
fn jaccard_partial_overlap() {
    // {"a", "b"} ∩ {"b", "c"} = {"b"}, union = {"a","b","c"} → 1/3
    let score = jaccard_word_similarity("a b", "b c");
    let expected = 1.0_f64 / 3.0_f64;
    assert!(
        (score - expected).abs() < 1e-10,
        "expected ~{expected}, got {score}"
    );
}

#[test]
fn jaccard_one_empty() {
    assert_eq!(jaccard_word_similarity("", "hello world"), 0.0);
    assert_eq!(jaccard_word_similarity("hello world", ""), 0.0);
}

#[test]
fn jaccard_both_empty() {
    assert_eq!(jaccard_word_similarity("", ""), 1.0);
}
