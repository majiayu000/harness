use super::{
    cancel_eval_workflow_family, cleanup_cancelled_eval_run, collect_eval_case_evidence,
    enqueue_eval_case_workflow, eval_report_from_evidence, finalize_eval_case_cleanup,
    EvalAttestationSummary, EvalBenchmarkCase, EvalBenchmarkManifest, EvalCaseEvidence,
    EvalEvidenceStatus, EvalRunCleanupInput, EvalRunReport,
};
use crate::runtime::{
    RuntimeJob, RuntimeJobStatus, WorkflowCommandStatus, WorkflowInstance, WorkflowRuntimeStore,
};
use harness_core::types::{Decision, Event, SessionId};
use harness_observe::event_store::EventStore;
use serde_json::json;
use std::time::{Duration, Instant};

pub const DEFAULT_EVAL_POLL_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_EVAL_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalExecuteConfig {
    pub run_id: String,
    pub project_id: String,
    pub k: u32,
    pub poll_interval: Duration,
    pub dispatch_timeout: Duration,
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
            dispatch_timeout: DEFAULT_EVAL_DISPATCH_TIMEOUT,
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
        if self.dispatch_timeout.is_zero() {
            anyhow::bail!("eval execute dispatch_timeout must be greater than zero");
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
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvalDispatchStop {
    Dispatched,
    Cancelled,
}

struct EvalCaseCleanupGuard {
    store: WorkflowRuntimeStore,
    eval_run_id: String,
    case: EvalBenchmarkCase,
    armed: bool,
}

impl EvalCaseCleanupGuard {
    fn new(store: &WorkflowRuntimeStore, eval_run_id: &str, case: &EvalBenchmarkCase) -> Self {
        Self {
            store: store.clone(),
            eval_run_id: eval_run_id.to_string(),
            case: case.clone(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for EvalCaseCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                eval_run_id = %self.eval_run_id,
                case_id = %self.case.case_id,
                "eval execution was dropped outside a Tokio runtime; automatic cleanup could not start"
            );
            return;
        };
        let store = self.store.clone();
        let eval_run_id = self.eval_run_id.clone();
        let case = self.case.clone();
        runtime.spawn(async move {
            if let Some(error) = cleanup_timed_out_case(
                &store,
                &eval_run_id,
                &case,
                "eval execution future was cancelled",
            )
            .await
            {
                tracing::error!(
                    eval_run_id,
                    case_id = %case.case_id,
                    %error,
                    "cancelled eval execution cleanup did not complete"
                );
            }
        });
    }
}

pub async fn execute_manifest(
    store: &WorkflowRuntimeStore,
    observe: &EventStore,
    manifest: &EvalBenchmarkManifest,
    config: EvalExecuteConfig,
) -> anyhow::Result<EvalRunReport> {
    let (_cancellation_tx, cancellation_rx) = tokio::sync::watch::channel(false);
    execute_manifest_with_cancellation(store, observe, manifest, config, cancellation_rx).await
}

pub async fn execute_manifest_with_cancellation(
    store: &WorkflowRuntimeStore,
    observe: &EventStore,
    manifest: &EvalBenchmarkManifest,
    config: EvalExecuteConfig,
    mut cancellation: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<EvalRunReport> {
    config.validate()?;
    let mut evidence = Vec::new();

    for case in &manifest.cases {
        if *cancellation.borrow() {
            break;
        }
        if case.replay_blocker().is_some() {
            continue;
        }

        let task_id = eval_case_task_id(&config.run_id, &case.case_id);
        let case_timeout = config.case_timeout(case);
        let resource_limits = effective_resource_limits(case, case_timeout)?;
        let input = super::EvalCaseWorkflowInput {
            eval_run_id: &config.run_id,
            case,
            project_id: &config.project_id,
            task_id: &task_id,
            additional_prompt: config.additional_prompt.as_deref(),
            timeout_secs: case_timeout.as_secs(),
            resource_limits: &resource_limits,
        };
        let enqueue = match enqueue_eval_case_workflow(store, input).await {
            Ok(enqueue) => enqueue,
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

        let workflow_id = enqueue.plan.workflow_id;
        let mut cleanup_guard = EvalCaseCleanupGuard::new(store, &config.run_id, case);
        let dispatch = enqueue
            .command_ids
            .first()
            .map(String::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "eval case {} did not create a runtime command",
                    case.case_id
                )
            });
        let dispatch = match dispatch {
            Ok(command_id) => {
                wait_for_eval_case_dispatch(
                    store,
                    command_id,
                    config.dispatch_timeout,
                    config.poll_interval,
                    &mut cancellation,
                )
                .await
            }
            Err(error) => Err(error),
        };
        if matches!(&dispatch, Ok(EvalDispatchStop::Cancelled)) {
            let cleanup_detail = cleanup_timed_out_case(
                store,
                &config.run_id,
                case,
                "eval execution was interrupted",
            )
            .await;
            cleanup_guard.disarm();
            let mut cancelled = synthetic_failure_evidence(
                &config.run_id,
                &case.case_id,
                Some(workflow_id),
                EvalEvidenceStatus::Skipped,
                "operator_cancelled",
                None,
            );
            if let Some(detail) = cleanup_detail {
                cancelled.status = EvalEvidenceStatus::EvidenceIncomplete;
                cancelled.missing_evidence.push(detail);
            }
            evidence.push(cancelled);
            break;
        }
        if let Err(error) = dispatch {
            let cleanup_detail = cleanup_timed_out_case(
                store,
                &config.run_id,
                case,
                "eval runtime dispatch was unavailable",
            )
            .await;
            cleanup_guard.disarm();
            let mut failure = synthetic_failure_evidence(
                &config.run_id,
                &case.case_id,
                Some(workflow_id),
                EvalEvidenceStatus::DispatchFailed,
                "dispatch_unavailable",
                Some(error.to_string()),
            );
            if let Some(detail) = cleanup_detail {
                failure.missing_evidence.push(detail);
            }
            evidence.push(failure);
            continue;
        }
        let stop = match wait_for_eval_case_stop(
            store,
            &workflow_id,
            case_timeout,
            config.poll_interval,
            &mut cancellation,
        )
        .await
        {
            Ok(stop) => stop,
            Err(error) => {
                let cleanup_detail =
                    cleanup_timed_out_case(store, &config.run_id, case, "eval case polling failed")
                        .await;
                cleanup_guard.disarm();
                let mut failure = synthetic_failure_evidence(
                    &config.run_id,
                    &case.case_id,
                    Some(workflow_id),
                    EvalEvidenceStatus::EvidenceIncomplete,
                    "wait_for_stop_failed",
                    Some(error.to_string()),
                );
                if let Some(detail) = cleanup_detail {
                    failure.missing_evidence.push(detail);
                }
                evidence.push(failure);
                continue;
            }
        };

        let was_cancelled = stop == EvalCaseStop::Cancelled;
        let cleanup_error = match &stop {
            EvalCaseStop::Stopped(_) => {
                finalize_eval_case_cleanup(store, &config.run_id, &case.case_id, &workflow_id)
                    .await
                    .err()
                    .map(|error| format!("cleanup_failed: {error}"))
            }
            EvalCaseStop::TimedOut => {
                cleanup_timed_out_case(store, &config.run_id, case, "eval case timed out").await
            }
            EvalCaseStop::Cancelled => {
                cleanup_timed_out_case(
                    store,
                    &config.run_id,
                    case,
                    "eval execution was interrupted",
                )
                .await
            }
        };
        let cleanup_failed = cleanup_error.is_some();
        cleanup_guard.disarm();

        let mut case_evidence = match collect_eval_case_evidence(
            store,
            &config.run_id,
            &case.case_id,
            &workflow_id,
            &case.base_commit,
        )
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
        if let Some(cleanup_error) = cleanup_error {
            push_missing_evidence(&mut case_evidence, &cleanup_error);
            case_evidence.status = EvalEvidenceStatus::EvidenceIncomplete;
        }
        match stop {
            EvalCaseStop::Stopped(state) => {
                if let Some(runtime) = case_evidence.runtime.as_mut() {
                    if runtime.terminal_state.is_none() {
                        runtime.terminal_state = Some(state);
                    }
                }
            }
            EvalCaseStop::TimedOut => {
                case_evidence.status = timed_out_evidence_status(cleanup_failed);
                push_missing_evidence(&mut case_evidence, "case_timeout");
            }
            EvalCaseStop::Cancelled => {
                if !cleanup_failed {
                    case_evidence.status = EvalEvidenceStatus::Skipped;
                }
                push_missing_evidence(&mut case_evidence, "operator_cancelled");
            }
        }
        evidence.push(case_evidence);
        if was_cancelled {
            break;
        }
    }

    let report = eval_report_from_evidence(manifest, config.run_id, config.k, evidence)?;
    if let Err(error) = emit_eval_events(observe, &report).await {
        tracing::error!(run_id = %report.run_id, %error, "eval report event persistence failed");
    }
    Ok(report)
}

async fn wait_for_eval_case_dispatch(
    store: &WorkflowRuntimeStore,
    command_id: &str,
    timeout: Duration,
    poll_interval: Duration,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<EvalDispatchStop> {
    let started = Instant::now();
    loop {
        let runtime_jobs = store.runtime_jobs_for_command(command_id).await?;
        if eval_runtime_job_started(&runtime_jobs) {
            return Ok(EvalDispatchStop::Dispatched);
        }
        let command = store
            .get_command(command_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("eval runtime command disappeared: {command_id}"))?;
        match (runtime_jobs.is_empty(), command.status) {
            (true, WorkflowCommandStatus::Pending | WorkflowCommandStatus::Dispatching)
            | (
                false,
                WorkflowCommandStatus::Pending
                | WorkflowCommandStatus::Dispatching
                | WorkflowCommandStatus::Dispatched,
            ) => {}
            (_, WorkflowCommandStatus::Deferred) => {
                anyhow::bail!("eval runtime command was deferred by dispatch policy")
            }
            (_, status) => anyhow::bail!(
                "eval runtime command reached {} without creating a runtime job",
                status.as_str()
            ),
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            anyhow::bail!(
                "no workflow runtime dispatcher claimed the eval command within {} seconds",
                timeout.as_secs()
            );
        }
        if wait_interval_or_cancel(
            cancellation,
            poll_interval.min(timeout.saturating_sub(elapsed)),
        )
        .await
        {
            return Ok(EvalDispatchStop::Cancelled);
        }
    }
}

fn eval_runtime_job_started(runtime_jobs: &[RuntimeJob]) -> bool {
    runtime_jobs
        .iter()
        .any(|job| job.status != RuntimeJobStatus::Pending)
}

fn timed_out_evidence_status(cleanup_failed: bool) -> EvalEvidenceStatus {
    if cleanup_failed {
        EvalEvidenceStatus::EvidenceIncomplete
    } else {
        EvalEvidenceStatus::TimedOut
    }
}

fn effective_resource_limits(
    case: &EvalBenchmarkCase,
    timeout: Duration,
) -> anyhow::Result<harness_sandbox::CappedResourceLimits> {
    let timeout_secs = timeout.as_secs();
    let requested = case
        .resource_limits
        .requested
        .overlay(harness_sandbox::ResourceLimits {
            cpu_time_secs: Some(timeout_secs),
            wall_time_secs: Some(timeout_secs),
            ..Default::default()
        });
    requested
        .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())
        .map_err(Into::into)
}

async fn cleanup_timed_out_case(
    store: &WorkflowRuntimeStore,
    eval_run_id: &str,
    case: &EvalBenchmarkCase,
    reason: &str,
) -> Option<String> {
    let workflow_id = format!("eval:{eval_run_id}:{}", case.case_id);
    if let Err(error) =
        cancel_eval_workflow_family(store, eval_run_id, &case.case_id, &workflow_id, reason).await
    {
        return Some(format!("cleanup_failed: {error}"));
    }
    match cleanup_cancelled_eval_run(
        store,
        EvalRunCleanupInput {
            eval_run_id,
            cases: std::slice::from_ref(case),
            reason,
        },
    )
    .await
    {
        Ok(summary) if summary.is_clean() => None,
        Ok(summary) => Some(format!("cleanup_incomplete: {summary:?}")),
        Err(error) => Some(format!("cleanup_failed: {error}")),
    }
}

async fn wait_for_eval_case_stop(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    timeout: Duration,
    poll_interval: Duration,
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
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

        if wait_interval_or_cancel(
            cancellation,
            poll_interval.min(timeout.saturating_sub(elapsed)),
        )
        .await
        {
            return Ok(EvalCaseStop::Cancelled);
        }
    }
}

async fn wait_interval_or_cancel(
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
    interval: Duration,
) -> bool {
    if *cancellation.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(interval) => false,
        changed = cancellation.changed() => changed.is_ok() && *cancellation.borrow(),
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

    #[test]
    fn pending_runtime_job_is_not_dispatched_until_a_worker_starts_it() {
        let mut job = RuntimeJob::pending(
            "command-1",
            crate::runtime::RuntimeKind::RemoteHost,
            "eval-host",
            json!({}),
        );
        assert!(!eval_runtime_job_started(std::slice::from_ref(&job)));
        job.claim("host-1", chrono::Utc::now() + chrono::Duration::minutes(1));
        assert!(eval_runtime_job_started(&[job]));
    }

    #[test]
    fn timeout_with_failed_cleanup_remains_infrastructure_incomplete() {
        assert_eq!(
            timed_out_evidence_status(true),
            EvalEvidenceStatus::EvidenceIncomplete
        );
        assert_eq!(
            timed_out_evidence_status(false),
            EvalEvidenceStatus::TimedOut
        );
    }

    #[tokio::test]
    async fn wait_interval_returns_immediately_after_cancellation() {
        let (cancellation_tx, mut cancellation_rx) = tokio::sync::watch::channel(false);
        assert!(cancellation_tx.send(true).is_ok());

        assert!(wait_interval_or_cancel(&mut cancellation_rx, Duration::from_secs(60)).await);
    }

    #[test]
    fn timeout_override_updates_effective_resource_deadlines() -> anyhow::Result<()> {
        let case = EvalBenchmarkCase {
            case_id: "case-1".to_string(),
            repo: "owner/repo".to_string(),
            issue: 1,
            base_commit: "abcdef1".to_string(),
            verify_commands: vec!["cargo test".to_string()],
            verify_command_mode: super::super::EvalVerifyCommandMode::Argv,
            paths: Vec::new(),
            risk: None,
            evidence: Vec::new(),
            resolution_prs: Vec::new(),
            resolution_commits: Vec::new(),
            commit_resolution: None,
            verdict: None,
            timeout_secs: 120,
            resource_limits: harness_sandbox::ResourceLimits::evaluation_defaults(120)
                .cap_by(harness_sandbox::ResourceLimits::operator_default_maxima())?,
            isolation: super::super::EvalIsolationProfile::default(),
        };

        let limits = effective_resource_limits(&case, Duration::from_secs(45))?;

        assert_eq!(limits.requested.cpu_time_secs, Some(45));
        assert_eq!(limits.requested.wall_time_secs, Some(45));
        assert_eq!(limits.effective.cpu_time_secs, Some(45));
        assert_eq!(limits.effective.wall_time_secs, Some(45));
        Ok(())
    }
}
