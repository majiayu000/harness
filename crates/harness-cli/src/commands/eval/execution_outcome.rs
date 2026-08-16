use harness_workflow::runtime::{EvalEventPersistenceError, EvalRunOutcome, EvalRunReport};

pub(super) struct EvalExecutionOutcome {
    pub(super) report: EvalRunReport,
    pub(super) interrupted: bool,
    pub(super) budget_exhausted: bool,
    pub(super) incomplete: bool,
    pub(super) execution_failure: Option<String>,
}

pub(super) fn execution_result(
    result: anyhow::Result<EvalRunReport>,
    interrupted: bool,
) -> anyhow::Result<EvalExecutionOutcome> {
    match result {
        Ok(report) => Ok(outcome(report, interrupted, None)),
        Err(error) => {
            let Some(failure) = error.downcast_ref::<EvalEventPersistenceError>() else {
                return Err(error);
            };
            Ok(outcome(
                failure.report().clone(),
                interrupted,
                Some(error.to_string()),
            ))
        }
    }
}

fn outcome(
    report: EvalRunReport,
    interrupted: bool,
    execution_failure: Option<String>,
) -> EvalExecutionOutcome {
    EvalExecutionOutcome {
        budget_exhausted: report
            .outcome
            .is_some_and(EvalRunOutcome::is_budget_exhausted),
        incomplete: report.outcome.is_some_and(EvalRunOutcome::is_incomplete),
        report,
        interrupted,
        execution_failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::eval::{eval_diff_regressions, EvalDiffArgs};
    use harness_workflow::runtime::diff_eval_run_reports;
    use harness_workflow::runtime::EvalReportMetrics;
    use std::path::PathBuf;

    fn report(outcome: Option<EvalRunOutcome>) -> EvalRunReport {
        EvalRunReport {
            run_id: "run-1".to_string(),
            suite: "suite".to_string(),
            k: 1,
            metrics: EvalReportMetrics {
                total_cases: 0,
                scored_cases: 0,
                passed_cases: 0,
                failed_cases: 0,
                skipped_cases: 0,
                pending_cases: 0,
                infra_failed_cases: 0,
                pass_at_1: 0.0,
                pass_to_k: 0.0,
                total_tokens: 0,
                avg_tokens_per_scored_case: None,
                total_cost_usd_micros: 0,
                avg_cost_usd_micros_per_scored_case: None,
            },
            outcome,
            cases: Vec::new(),
        }
    }

    #[test]
    fn budget_outcome_is_preserved() {
        let outcome = execution_result(Ok(report(Some(EvalRunOutcome::BudgetExhausted))), false)
            .expect("outcome should build");

        assert!(outcome.budget_exhausted);
        assert!(!outcome.incomplete);
        assert!(!outcome.interrupted);
    }

    #[test]
    fn incomplete_outcome_is_preserved() {
        let outcome = execution_result(Ok(report(Some(EvalRunOutcome::Incomplete))), false)
            .expect("outcome should build");

        assert!(outcome.incomplete);
        assert!(!outcome.budget_exhausted);
    }

    #[test]
    fn event_failure_retains_partial_report_for_output() {
        let error = EvalEventPersistenceError::new(
            anyhow::anyhow!("event store unavailable"),
            report(None),
        );

        let outcome =
            execution_result(Err(error.into()), false).expect("partial report should be retained");

        assert_eq!(outcome.report.run_id, "run-1");
        assert_eq!(
            outcome.report.outcome,
            Some(EvalRunOutcome::EventPersistenceFailed)
        );
        assert!(outcome
            .execution_failure
            .as_deref()
            .is_some_and(|error| error.contains("event store unavailable")));
    }

    #[test]
    fn diff_gate_rejects_budget_exhausted_candidate() {
        let baseline = report(None);
        let candidate = report(Some(EvalRunOutcome::BudgetExhausted));
        let diff = diff_eval_run_reports(&baseline, &candidate);
        let args = EvalDiffArgs {
            baseline: PathBuf::new(),
            candidate: PathBuf::new(),
            max_pass_drop: None,
            fail_on_new_f_gate: false,
            json: false,
            output: None,
        };

        let regressions = eval_diff_regressions(&baseline, &candidate, &diff, &args)
            .expect("gate evaluation should succeed");

        assert!(regressions
            .iter()
            .any(|regression| regression.contains("candidate run is incomplete")));
    }

    #[test]
    fn diff_gate_rejects_infrastructure_incomplete_candidate() {
        let baseline = report(None);
        let mut candidate = report(None);
        candidate.metrics.total_cases = 1;
        candidate.metrics.infra_failed_cases = 1;
        let diff = diff_eval_run_reports(&baseline, &candidate);
        let args = EvalDiffArgs {
            baseline: PathBuf::new(),
            candidate: PathBuf::new(),
            max_pass_drop: Some(0.0),
            fail_on_new_f_gate: true,
            json: false,
            output: None,
        };

        let regressions = eval_diff_regressions(&baseline, &candidate, &diff, &args)
            .expect("gate evaluation should succeed");

        assert!(regressions
            .iter()
            .any(|regression| regression.contains("candidate run is incomplete")));
    }
}
