use super::super::EvalCaseEvidence;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalUsageCeiling {
    pub max_total_tokens: Option<u64>,
    pub max_cost_usd_micros: Option<u64>,
}

impl EvalUsageCeiling {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if self.max_total_tokens == Some(0) {
            anyhow::bail!("eval suite max_total_tokens must be greater than zero");
        }
        if self.max_cost_usd_micros == Some(0) {
            anyhow::bail!("eval suite max_cost_usd_micros must be greater than zero");
        }
        if self.max_total_tokens.is_none() && self.max_cost_usd_micros.is_none() {
            anyhow::bail!("eval suite usage ceiling must set at least one limit");
        }
        Ok(())
    }

    fn exhaustion_reason(&self, total_tokens: u64, cost_usd_micros: u64) -> Option<String> {
        let token_limit_reached = self
            .max_total_tokens
            .is_some_and(|limit| total_tokens >= limit);
        let cost_limit_reached = self
            .max_cost_usd_micros
            .is_some_and(|limit| cost_usd_micros >= limit);
        if !token_limit_reached && !cost_limit_reached {
            return None;
        }

        Some(format!(
            "suite usage ceiling reached: total_tokens={total_tokens}, max_total_tokens={}, cost_usd_micros={cost_usd_micros}, max_cost_usd_micros={}",
            optional_limit(self.max_total_tokens),
            optional_limit(self.max_cost_usd_micros)
        ))
    }
}

pub(super) fn suite_usage_exhaustion_reason(
    ceiling: Option<&EvalUsageCeiling>,
    evidence: &[EvalCaseEvidence],
) -> Option<String> {
    let ceiling = ceiling?;
    let (total_tokens, cost_usd_micros) =
        evidence
            .iter()
            .fold((0_u64, 0_u64), |(total_tokens, cost_usd_micros), case| {
                case.usage.iter().fold(
                    (total_tokens, cost_usd_micros),
                    |(total_tokens, cost_usd_micros), usage| {
                        let tokens = usage.derived_total_tokens().unwrap_or(0);
                        (
                            total_tokens.saturating_add(tokens),
                            cost_usd_micros.saturating_add(usage.cost_usd_micros.unwrap_or(0)),
                        )
                    },
                )
            });
    ceiling.exhaustion_reason(total_tokens, cost_usd_micros)
}

fn optional_limit(limit: Option<u64>) -> String {
    limit
        .map(|limit| limit.to_string())
        .unwrap_or_else(|| "unlimited".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        eval::model::{Confidence, UsageSnapshot},
        EvalAttestationSummary, EvalEvidenceStatus,
    };

    fn case_evidence(total_tokens: u64, cost_usd_micros: u64) -> EvalCaseEvidence {
        EvalCaseEvidence {
            eval_run_id: "run-1".to_string(),
            case_id: "case-1".to_string(),
            workflow_id: None,
            status: EvalEvidenceStatus::Failed,
            attestation: EvalAttestationSummary::unsigned(),
            runtime: None,
            usage: vec![UsageSnapshot {
                agent_invocation_id: None,
                runtime_job_id: None,
                workflow_id: None,
                model: None,
                reasoning_effort: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(total_tokens),
                cost_usd_micros: Some(cost_usd_micros),
                token_confidence: Confidence::Observed,
                cost_confidence: Confidence::Observed,
            }],
            submission: None,
            quality_gate: None,
            quality: None,
            isolation: None,
            missing_evidence: Vec::new(),
        }
    }

    #[test]
    fn stops_at_token_or_cost_limit() {
        let ceiling = EvalUsageCeiling {
            max_total_tokens: Some(100),
            max_cost_usd_micros: Some(50),
        };

        assert!(ceiling.exhaustion_reason(99, 49).is_none());
        assert!(ceiling
            .exhaustion_reason(100, 1)
            .is_some_and(|reason| reason.contains("total_tokens=100")));
        assert!(ceiling
            .exhaustion_reason(1, 50)
            .is_some_and(|reason| reason.contains("cost_usd_micros=50")));
    }

    #[test]
    fn rejects_zero_or_empty_limits() {
        assert!(EvalUsageCeiling::default().validate().is_err());
        assert!(EvalUsageCeiling {
            max_total_tokens: Some(0),
            max_cost_usd_micros: None,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn accumulated_case_evidence_stops_the_next_dispatch() {
        let ceiling = EvalUsageCeiling {
            max_total_tokens: Some(100),
            max_cost_usd_micros: None,
        };
        let evidence = vec![case_evidence(40, 10), case_evidence(60, 20)];

        let reason = suite_usage_exhaustion_reason(Some(&ceiling), &evidence)
            .expect("the next case must not be dispatched");

        assert!(reason.contains("total_tokens=100"));
    }

    #[test]
    fn final_completed_case_crossing_the_ceiling_is_detected() {
        let ceiling = EvalUsageCeiling {
            max_total_tokens: Some(100),
            max_cost_usd_micros: None,
        };
        let evidence = vec![case_evidence(101, 0)];

        assert!(suite_usage_exhaustion_reason(Some(&ceiling), &evidence).is_some());
    }
}
