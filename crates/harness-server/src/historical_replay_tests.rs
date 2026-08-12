use harness_workflow::runtime::parse_historical_replay_cohort_str;

const ASC_030_COHORT: &str = include_str!("../../../evals/historical-replay/asc-030-cohort.json");

#[test]
fn historical_replay_asc030_server_cases_have_retained_evidence() {
    let cohort =
        parse_historical_replay_cohort_str(ASC_030_COHORT).expect("ASC-030 cohort should validate");

    let server_cases = cohort
        .cases
        .iter()
        .filter(|case| case.replay.command.contains("-p harness-server"))
        .collect::<Vec<_>>();
    assert_eq!(server_cases.len(), 2);

    for case in server_cases {
        assert_eq!(case.issue.state, "closed");
        assert_eq!(case.closing_pr.state, "merged");
        assert_eq!(case.replay.baseline.tests_run, 0);
        assert!(case.replay.candidate.tests_run > 0);
        assert!(case.comparison.infrastructure_failures.is_empty());
    }
}
