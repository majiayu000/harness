use super::{
    collect_eval_case_evidence, dispatch_eval_case_workflow, eval_isolated_runtime_profile,
    eval_report_from_evidence, EvalAttestationSummary, EvalBenchmarkCase, EvalBenchmarkManifest,
    EvalCaseEvidence, EvalEvidenceStatus, EvalRunReport,
};
use crate::runtime::{WorkflowInstance, WorkflowRuntimeStore};
use harness_core::types::{Decision, Event, SessionId};
use harness_observe::event_store::EventStore;
use serde_json::json;
use std::time::{Duration, Instant};

pub const DEFAULT_EVAL_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalExecuteConfig {
    pub run_id: String,
    pub project_id: String,
    pub k: u32,
    pub poll_interval: Duration,
    pub case_timeout_override: Option<Duration>,
    pub additional_prompt: Option<String>,
}

impl EvalExecuteConfig {
    pub fn new(run_id: impl Into<String>, project_id: impl Into<String>, k: u32) -> Self {
        Self {
            run_id: run_id.into(),
            project_id: project_id.into(),
            k,
            poll_interval: DEFAULT_EVAL_POLL_INTERVAL,
            case_timeout_override: None,
            additional_prompt: None,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.run_id.trim().is_empty() {
            anyhow::bail!("eval execute run_id must not be empty");
        }
        if self.project_id.trim().is_empty() {
            anyhow::bail!("eval execute project_id must not be empty");
        }
        if self.poll_interval.is_zero() {
            anyhow::bail!("eval execute poll_interval must be greater than zero");
        }
        if self
            .case_timeout_override
            .is_some_and(|timeout| timeout.is_zero())
        {
            anyhow::bail!("eval execute case_timeout_override must be greater than zero");
        }
        Ok(())
    }

    fn case_timeout(&self, case: &EvalBenchmarkCase) -> Duration {
        self.case_timeout_override
            .unwrap_or_else(|| Duration::from_secs(case.timeout_secs))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EvalCaseStop {
    Stopped(String),
    TimedOut,
}

pub async fn execute_manifest(
    store: &WorkflowRuntimeStore,
    observe: &EventStore,
    manifest: &EvalBenchmarkManifest,
    config: EvalExecuteConfig,
) -> anyhow::Result<EvalRunReport> {
    config.validate()?;
    let mut evidence = Vec::new();

    for case in &manifest.cases {
        if case.replay_blocker().is_some() {
            continue;
        }

        let task_id = eval_case_task_id(&config.run_id, &case.case_id);
        let input = super::EvalCaseWorkflowInput {
            eval_run_id: &config.run_id,
            case,
            project_id: &config.project_id,
            task_id: &task_id,
            additional_prompt: config.additional_prompt.as_deref(),
        };
        let dispatch =
            dispatch_eval_case_workflow(store, eval_isolated_runtime_profile(case), input).await;
        let dispatch = match dispatch {
            Ok(dispatch) => dispatch,
            Err(error) => {
                evidence.push(synthetic_failure_evidence(
                    &config.run_id,
                    &case.case_id,
                    None,
                    EvalEvidenceStatus::DispatchFailed,
                    "dispatch_failed",
                    Some(error.to_string()),
                ));
                continue;
            }
        };

        let workflow_id = dispatch.enqueue.plan.workflow_id;
        let stop = wait_for_eval_case_stop(
            store,
            &workflow_id,
            config.case_timeout(case),
            config.poll_interval,
        )
        .await?;
        let mut case_evidence =
            match collect_eval_case_evidence(store, &config.run_id, &case.case_id, &workflow_id)
                .await
            {
                Ok(evidence) => evidence,
                Err(error) => synthetic_failure_evidence(
                    &config.run_id,
                    &case.case_id,
                    Some(workflow_id.clone()),
                    EvalEvidenceStatus::EvidenceIncomplete,
                    "evidence_collection_failed",
                    Some(error.to_string()),
                ),
            };
        match stop {
            EvalCaseStop::Stopped(state) => {
                if case_evidence.status == EvalEvidenceStatus::Failed
                    && !case_evidence.missing_evidence.is_empty()
                {
                    case_evidence.status = EvalEvidenceStatus::EvidenceIncomplete;
                }
                if let Some(runtime) = case_evidence.runtime.as_mut() {
                    if runtime.terminal_state.is_none() {
                        runtime.terminal_state = Some(state);
                    }
                }
            }
            EvalCaseStop::TimedOut => {
                case_evidence.status = EvalEvidenceStatus::TimedOut;
                push_missing_evidence(&mut case_evidence, "case_timeout");
            }
        }
        evidence.push(case_evidence);
    }

    let report = eval_report_from_evidence(manifest, config.run_id, config.k, evidence)?;
    emit_eval_events(observe, &report).await?;
    Ok(report)
}

async fn wait_for_eval_case_stop(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> anyhow::Result<EvalCaseStop> {
    let started = Instant::now();
    loop {
        if let Some(instance) = store.get_instance(workflow_id).await? {
            if eval_case_stopped(&instance) {
                return Ok(EvalCaseStop::Stopped(instance.state));
            }
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Ok(EvalCaseStop::TimedOut);
        }

        tokio::time::sleep(poll_interval.min(timeout.saturating_sub(elapsed))).await;
    }
}

fn eval_case_stopped(instance: &WorkflowInstance) -> bool {
    instance.is_terminal() || eval_case_stopped_state(&instance.state)
}

fn eval_case_stopped_state(state: &str) -> bool {
    matches!(
        state,
        "ready_to_merge" | "blocked" | "failed" | "cancelled" | "canceled"
    )
}

fn synthetic_failure_evidence(
    eval_run_id: &str,
    case_id: &str,
    workflow_id: Option<String>,
    status: EvalEvidenceStatus,
    missing_key: &str,
    detail: Option<String>,
) -> EvalCaseEvidence {
    let mut missing_evidence = vec![missing_key.to_string()];
    if let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) {
        missing_evidence.push(format!("{missing_key}: {detail}"));
    }
    EvalCaseEvidence {
        eval_run_id: eval_run_id.to_string(),
        case_id: case_id.to_string(),
        workflow_id,
        status,
        attestation: EvalAttestationSummary::unsigned(),
        runtime: None,
        usage: Vec::new(),
        submission: None,
        quality_gate: None,
        quality: None,
        isolation: None,
        missing_evidence,
    }
}

fn push_missing_evidence(evidence: &mut EvalCaseEvidence, key: &str) {
    if !evidence
        .missing_evidence
        .iter()
        .any(|missing| missing == key)
    {
        evidence.missing_evidence.push(key.to_string());
    }
}

fn eval_case_task_id(eval_run_id: &str, case_id: &str) -> String {
    format!("eval-run:{eval_run_id}:{case_id}")
}

async fn emit_eval_events(observe: &EventStore, report: &EvalRunReport) -> anyhow::Result<()> {
    let session = SessionId::from_str(&format!("eval:{}", report.run_id));
    let mut events = Vec::with_capacity(report.cases.len() + 1);
    for case in &report.cases {
        let mut event = Event::new(
            session.clone(),
            "eval_case_scored",
            "harness_eval",
            if case.passed {
                Decision::Pass
            } else {
                Decision::Block
            },
        );
        event.reason = Some(format!("{} status {:?}", case.case_id, case.status));
        event.content = Some(serde_json::to_string(&json!({
            "suite": &report.suite,
            "run_id": &report.run_id,
            "case_id": &case.case_id,
            "repo": &case.repo,
            "issue": case.issue,
            "status": case.status,
            "passed": case.passed,
            "grade": case.final_grade,
            "failed_gates": case.failed_hard_gates.iter().map(|gate| format!("{:?}", gate.name)).collect::<Vec<_>>(),
            "total_tokens": case.total_tokens,
            "cost_usd_micros": case.cost_usd_micros,
            "workflow_id": &case.workflow_id,
            "terminal_state": &case.terminal_state,
        }))?);
        events.push(event);
    }

    let mut run_event = Event::new(
        session,
        "eval_run_completed",
        "harness_eval",
        Decision::Complete,
    );
    run_event.reason = Some(format!(
        "{} completed with {} passed, {} failed, {} infra failed",
        report.run_id,
        report.metrics.passed_cases,
        report.metrics.failed_cases,
        report.metrics.infra_failed_cases
    ));
    run_event.content = Some(serde_json::to_string(&json!({
        "suite": &report.suite,
        "run_id": &report.run_id,
        "k": report.k,
        "pass_at_1": report.metrics.pass_at_1,
        "pass_to_k": report.metrics.pass_to_k,
        "passed_cases": report.metrics.passed_cases,
        "failed_cases": report.metrics.failed_cases,
        "infra_failed_cases": report.metrics.infra_failed_cases,
        "total_cases": report.metrics.total_cases,
        "total_tokens": report.metrics.total_tokens,
        "total_cost_usd_micros": report.metrics.total_cost_usd_micros,
    }))?);
    events.push(run_event);

    observe.log_many(&events).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_case_stopped_state_includes_eval_terminal_states() {
        for state in [
            "ready_to_merge",
            "blocked",
            "failed",
            "cancelled",
            "canceled",
        ] {
            assert!(eval_case_stopped_state(state));
        }
        for state in ["implementing", "awaiting_feedback", "quality_gate_pending"] {
            assert!(!eval_case_stopped_state(state));
        }
    }

    #[test]
    fn synthetic_failure_evidence_records_distinct_status_and_reason() {
        let evidence = synthetic_failure_evidence(
            "run-1",
            "case-1",
            Some("workflow-1".to_string()),
            EvalEvidenceStatus::TimedOut,
            "case_timeout",
            Some("elapsed".to_string()),
        );

        assert_eq!(evidence.status, EvalEvidenceStatus::TimedOut);
        assert_eq!(evidence.workflow_id.as_deref(), Some("workflow-1"));
        assert!(evidence
            .missing_evidence
            .contains(&"case_timeout".to_string()));
        assert!(evidence
            .missing_evidence
            .contains(&"case_timeout: elapsed".to_string()));
    }
}
