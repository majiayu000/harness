use super::{
    collect_eval_case_evidence, eval_isolated_runtime_profile, eval_report_from_evidence,
    EvalBenchmarkCase, EvalBenchmarkManifest, EvalCaseEvidence, EvalEvidenceStatus, EvalRunReport,
};
use crate::runtime::eval::run::{
    cleanup_cancelled_eval_run, dispatch_eval_case_workflow, EvalCaseWorkflowInput,
    EvalRunCleanupInput,
};
use crate::runtime::WorkflowRuntimeStore;
use std::time::Duration;

const DEFAULT_EVAL_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalExecuteConfig {
    pub run_id: String,
    pub k: u32,
    pub project_id: String,
    pub poll_interval: Duration,
    pub additional_prompt: Option<String>,
}

impl EvalExecuteConfig {
    pub fn new(run_id: impl Into<String>, k: u32, project_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            k,
            project_id: project_id.into(),
            poll_interval: DEFAULT_EVAL_POLL_INTERVAL,
            additional_prompt: None,
        }
    }
}

pub async fn execute_eval_manifest(
    store: &WorkflowRuntimeStore,
    manifest: &EvalBenchmarkManifest,
    config: EvalExecuteConfig,
) -> anyhow::Result<EvalRunReport> {
    let mut evidence = Vec::new();
    for (index, case) in manifest.cases.iter().enumerate() {
        if let Some(blocker) = case.replay_blocker() {
            tracing::warn!(
                eval_run_id = %config.run_id,
                case_id = %case.case_id,
                blocker,
                "skipping non-replayable eval case"
            );
            continue;
        }
        evidence.push(execute_eval_case(store, case, &config, index).await);
    }
    eval_report_from_evidence(manifest, config.run_id, config.k, evidence)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

async fn execute_eval_case(
    store: &WorkflowRuntimeStore,
    case: &EvalBenchmarkCase,
    config: &EvalExecuteConfig,
    case_index: usize,
) -> EvalCaseEvidence {
    let task_id = eval_case_task_id(&config.run_id, case_index);
    let input = EvalCaseWorkflowInput {
        eval_run_id: &config.run_id,
        case,
        project_id: &config.project_id,
        task_id: &task_id,
        additional_prompt: config.additional_prompt.as_deref(),
    };
    let outcome = match dispatch_eval_case_workflow(
        store,
        eval_isolated_runtime_profile(case),
        input,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            return infrastructure_failure_evidence(
                &config.run_id,
                &case.case_id,
                format!("dispatch_failed: {error}"),
            );
        }
    };

    let workflow_id = outcome.enqueue.plan.workflow_id;
    if let Err(error) = wait_for_terminal_workflow(
        store,
        &workflow_id,
        Duration::from_secs(case.timeout_secs),
        config.poll_interval,
    )
    .await
    {
        let cleanup_error = cleanup_cancelled_eval_run(
            store,
            EvalRunCleanupInput {
                eval_run_id: &config.run_id,
                cases: std::slice::from_ref(case),
                reason: "eval case timed out",
            },
        )
        .await
        .err()
        .map(|error| format!("cleanup_failed: {error}"));
        let mut evidence =
            collect_or_infra_failure(store, &config.run_id, &case.case_id, &workflow_id).await;
        evidence.status = EvalEvidenceStatus::Failed;
        push_missing_evidence(&mut evidence, format!("timeout: {error}"));
        if let Some(cleanup_error) = cleanup_error {
            push_missing_evidence(&mut evidence, cleanup_error);
        }
        return evidence;
    }

    collect_or_infra_failure(store, &config.run_id, &case.case_id, &workflow_id).await
}

async fn wait_for_terminal_workflow(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match store.get_instance(workflow_id).await? {
            Some(workflow) if workflow.is_terminal() => return Ok(()),
            Some(_) => {}
            None => anyhow::bail!("workflow instance disappeared before reaching a terminal state"),
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "workflow did not reach a terminal state within {}s",
                timeout.as_secs()
            );
        }
        tokio::time::sleep((deadline - now).min(poll_interval)).await;
    }
}

async fn collect_or_infra_failure(
    store: &WorkflowRuntimeStore,
    run_id: &str,
    case_id: &str,
    workflow_id: &str,
) -> EvalCaseEvidence {
    match collect_eval_case_evidence(store, run_id, case_id, workflow_id).await {
        Ok(evidence) => evidence,
        Err(error) => infrastructure_failure_evidence(
            run_id,
            case_id,
            format!("evidence_collection_failed: {error}"),
        ),
    }
}

fn infrastructure_failure_evidence(
    run_id: &str,
    case_id: &str,
    failure: impl Into<String>,
) -> EvalCaseEvidence {
    EvalCaseEvidence {
        eval_run_id: run_id.to_string(),
        case_id: case_id.to_string(),
        workflow_id: None,
        status: EvalEvidenceStatus::Failed,
        attestation: super::EvalAttestationSummary::unsigned(),
        runtime: None,
        usage: Vec::new(),
        submission: None,
        quality_gate: None,
        quality: None,
        isolation: None,
        missing_evidence: vec!["workflow_instance".to_string(), failure.into()],
    }
}

fn push_missing_evidence(evidence: &mut EvalCaseEvidence, item: String) {
    if !evidence.missing_evidence.contains(&item) {
        evidence.missing_evidence.push(item);
    }
}

fn eval_case_task_id(run_id: &str, case_index: usize) -> String {
    let sanitized = run_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let run_id = if sanitized.is_empty() {
        "run"
    } else {
        sanitized.as_str()
    };
    format!("eval-{run_id}-case-{}", case_index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::eval::manifest::EvalIsolationProfile;
    use crate::runtime::eval::model::UsageSnapshot;
    use crate::runtime::{EvalCaseInfrastructureStatus, EvalReportCaseStatus, EvalRunReport};
    use harness_sandbox::ResourceLimits;

    fn case(case_id: &str) -> EvalBenchmarkCase {
        EvalBenchmarkCase {
            case_id: case_id.to_string(),
            repo: "owner/repo".to_string(),
            issue: 42,
            base_commit: "abcdef1".to_string(),
            verify_commands: vec!["cargo test".to_string()],
            paths: Vec::new(),
            risk: None,
            evidence: Vec::new(),
            resolution_prs: Vec::new(),
            resolution_commits: Vec::new(),
            commit_resolution: None,
            verdict: None,
            timeout_secs: 1,
            resource_limits: ResourceLimits::evaluation_defaults(1)
                .cap_by(ResourceLimits::operator_default_maxima())
                .expect("default resource limits should be valid"),
            isolation: EvalIsolationProfile::default(),
        }
    }

    fn manifest() -> EvalBenchmarkManifest {
        EvalBenchmarkManifest {
            suite: "suite".to_string(),
            cases: vec![case("case-pass"), case("case-fail")],
        }
    }

    fn evidence(
        case_id: &str,
        status: EvalEvidenceStatus,
        missing: Vec<String>,
    ) -> EvalCaseEvidence {
        EvalCaseEvidence {
            eval_run_id: "run-1".to_string(),
            case_id: case_id.to_string(),
            workflow_id: Some(format!("workflow-{case_id}")),
            status,
            attestation: super::super::EvalAttestationSummary::unsigned(),
            runtime: None,
            usage: vec![UsageSnapshot {
                agent_invocation_id: None,
                runtime_job_id: Some("job-1".to_string()),
                workflow_id: Some(format!("workflow-{case_id}")),
                model: None,
                reasoning_effort: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                total_tokens: Some(10),
                cost_usd_micros: Some(5),
                token_confidence: crate::runtime::eval::model::Confidence::Observed,
                cost_confidence: crate::runtime::eval::model::Confidence::Estimated,
            }],
            submission: None,
            quality_gate: None,
            quality: None,
            isolation: None,
            missing_evidence: missing,
        }
    }

    #[test]
    fn eval_execute_reports_pass_and_failure_evidence() {
        let report = report(vec![
            evidence("case-pass", EvalEvidenceStatus::Passed, Vec::new()),
            evidence(
                "case-fail",
                EvalEvidenceStatus::Failed,
                vec!["quality_gate_pass".to_string()],
            ),
        ]);

        assert_eq!(report.metrics.scored_cases, 2);
        assert_eq!(report.metrics.passed_cases, 1);
        assert_eq!(report.metrics.failed_cases, 1);
        assert_eq!(report.cases[0].status, EvalReportCaseStatus::Passed);
        assert_eq!(report.cases[1].status, EvalReportCaseStatus::Failed);
    }

    #[test]
    fn eval_execute_timeout_scores_as_failure_not_skip() {
        let mut timed_out = evidence("case-pass", EvalEvidenceStatus::Passed, Vec::new());
        timed_out.status = EvalEvidenceStatus::Failed;
        push_missing_evidence(&mut timed_out, "timeout: elapsed".to_string());

        let report = eval_report_from_evidence(
            &EvalBenchmarkManifest {
                suite: "suite".to_string(),
                cases: vec![case("case-pass")],
            },
            "run-1",
            1,
            vec![timed_out],
        )
        .expect("timeout evidence should report");

        assert_eq!(report.cases[0].status, EvalReportCaseStatus::Failed);
        assert!(!report.cases[0].passed);
        assert_eq!(report.metrics.skipped_cases, 0);
    }

    #[test]
    fn eval_execute_dispatch_failure_is_infrastructure_failure() {
        let failed =
            infrastructure_failure_evidence("run-1", "case-pass", "dispatch_failed: unavailable");
        let report = eval_report_from_evidence(
            &EvalBenchmarkManifest {
                suite: "suite".to_string(),
                cases: vec![case("case-pass")],
            },
            "run-1",
            1,
            vec![failed],
        )
        .expect("infrastructure evidence should report");

        assert_eq!(report.cases[0].status, EvalReportCaseStatus::InfraFailed);
        assert_eq!(
            report.cases[0].infrastructure_status,
            EvalCaseInfrastructureStatus::InfraFailed
        );
    }

    #[test]
    fn eval_case_task_id_is_branch_safe() {
        assert_eq!(
            eval_case_task_id("run:with/slashes#1", 0),
            "eval-run-with-slashes-1-case-1"
        );
    }

    fn report(evidence: Vec<EvalCaseEvidence>) -> EvalRunReport {
        eval_report_from_evidence(&manifest(), "run-1", 1, evidence)
            .expect("evidence should report")
    }
}
