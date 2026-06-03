use super::*;

#[test]
fn burn_level_marks_stale_running_job_high() {
    assert_eq!(
        burn_level("running", "implement_issue", Some("low"), 30, true),
        "high"
    );
}

#[test]
fn usage_aggregate_requires_configured_price_for_cost() {
    let mut usage = UsageAggregate::default();
    usage.add(
        &UsageMetrics {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reported_total_tokens: None,
        },
        None,
    );
    assert_eq!(usage.total_tokens(), 1_000_000);
    assert_eq!(usage.estimated_cost_json(true), None);
}
