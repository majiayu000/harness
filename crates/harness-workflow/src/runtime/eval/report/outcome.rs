use serde::{Deserialize, Serialize};

use super::{EvalReportCase, EvalReportCaseStatus, EvalReportMetrics, EvalRunReport};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunOutcome {
    BudgetExhausted,
    Incomplete,
    EventPersistenceFailed,
    BudgetExhaustedAndEventPersistenceFailed,
    IncompleteAndEventPersistenceFailed,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalReportCaseOutcome {
    BudgetExhausted,
}

impl EvalRunOutcome {
    pub fn is_budget_exhausted(self) -> bool {
        matches!(
            self,
            Self::BudgetExhausted | Self::BudgetExhaustedAndEventPersistenceFailed
        )
    }

    pub fn has_event_persistence_failure(self) -> bool {
        matches!(
            self,
            Self::EventPersistenceFailed
                | Self::BudgetExhaustedAndEventPersistenceFailed
                | Self::IncompleteAndEventPersistenceFailed
        )
    }

    pub fn is_incomplete(self) -> bool {
        matches!(
            self,
            Self::Incomplete | Self::IncompleteAndEventPersistenceFailed
        )
    }

    pub fn with_event_persistence_failure(current: Option<Self>) -> Self {
        match current {
            Some(Self::BudgetExhausted | Self::BudgetExhaustedAndEventPersistenceFailed) => {
                Self::BudgetExhaustedAndEventPersistenceFailed
            }
            Some(Self::Incomplete | Self::IncompleteAndEventPersistenceFailed) => {
                Self::IncompleteAndEventPersistenceFailed
            }
            Some(Self::EventPersistenceFailed) | None => Self::EventPersistenceFailed,
        }
    }

    pub fn after_event_retry(self) -> Option<Self> {
        match self {
            Self::BudgetExhaustedAndEventPersistenceFailed => Some(Self::BudgetExhausted),
            Self::IncompleteAndEventPersistenceFailed => Some(Self::Incomplete),
            Self::EventPersistenceFailed => None,
            Self::BudgetExhausted => Some(Self::BudgetExhausted),
            Self::Incomplete => Some(Self::Incomplete),
        }
    }
}

pub fn eval_report_effective_outcome(report: &EvalRunReport) -> Option<EvalRunOutcome> {
    let inferred = inferred_run_outcome(&report.cases, &report.metrics);
    match report.outcome {
        Some(EvalRunOutcome::EventPersistenceFailed) => {
            Some(EvalRunOutcome::with_event_persistence_failure(inferred))
        }
        Some(outcome) => Some(outcome),
        None => inferred,
    }
}

pub(super) fn inferred_run_outcome(
    cases: &[EvalReportCase],
    metrics: &EvalReportMetrics,
) -> Option<EvalRunOutcome> {
    if cases
        .iter()
        .any(|case| case.outcome == Some(EvalReportCaseOutcome::BudgetExhausted))
    {
        Some(EvalRunOutcome::BudgetExhausted)
    } else if metrics.pending_cases != 0
        || metrics.skipped_cases != 0
        || metrics.infra_failed_cases != 0
        || cases.iter().any(|case| {
            matches!(
                case.status,
                EvalReportCaseStatus::Pending
                    | EvalReportCaseStatus::Skipped
                    | EvalReportCaseStatus::InfraFailed
            )
        })
    {
        Some(EvalRunOutcome::Incomplete)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_failure_and_retry_preserve_budget_state() {
        let failed =
            EvalRunOutcome::with_event_persistence_failure(Some(EvalRunOutcome::BudgetExhausted));

        assert_eq!(
            failed,
            EvalRunOutcome::BudgetExhaustedAndEventPersistenceFailed
        );
        assert!(failed.has_event_persistence_failure());
        assert!(failed.is_budget_exhausted());
        assert_eq!(
            failed.after_event_retry(),
            Some(EvalRunOutcome::BudgetExhausted)
        );
    }

    #[test]
    fn event_failure_and_retry_preserve_incomplete_state() {
        let failed =
            EvalRunOutcome::with_event_persistence_failure(Some(EvalRunOutcome::Incomplete));

        assert_eq!(failed, EvalRunOutcome::IncompleteAndEventPersistenceFailed);
        assert!(failed.has_event_persistence_failure());
        assert!(failed.is_incomplete());
        assert_eq!(failed.after_event_retry(), Some(EvalRunOutcome::Incomplete));
    }
}
