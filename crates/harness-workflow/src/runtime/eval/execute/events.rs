use super::super::{eval_report_effective_outcome, EvalRunReport};
use harness_core::types::{Decision, Event, EventId, SessionId};
use harness_observe::event_store::EventStore;
use serde_json::json;
use sha2::{Digest, Sha256};

pub(super) async fn emit_eval_events(
    observe: &EventStore,
    report: &EvalRunReport,
) -> anyhow::Result<()> {
    let events = build_eval_events(report)?;
    observe.log_many(&events).await?;
    Ok(())
}

fn build_eval_events(report: &EvalRunReport) -> anyhow::Result<Vec<Event>> {
    let outcome = eval_report_effective_outcome(report);
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
        event.id = stable_event_id(&[
            "case",
            report.suite.as_str(),
            report.run_id.as_str(),
            case.case_id.as_str(),
        ]);
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
        if outcome.is_some() {
            Decision::Block
        } else {
            Decision::Complete
        },
    );
    run_event.id = stable_event_id(&["run", report.suite.as_str(), report.run_id.as_str()]);
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
        "outcome": outcome,
    }))?);
    events.push(run_event);

    Ok(events)
}

fn stable_event_id(parts: &[&str]) -> EventId {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    EventId::from_str(&format!("eval:{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::eval::{eval_report_dry_run, parse_benchmark_manifest_str};
    use crate::runtime::{EvalEventPersistenceError, EvalRunOutcome};
    use harness_core::types::EventFilters;

    fn report() -> EvalRunReport {
        let manifest = parse_benchmark_manifest_str(
            r#"
suite = "event-contract"

[[cases]]
case_id = "case-1"
repo = "owner/repo"
issue = 1
base_commit = "abcdef1"
verify_commands = ["cargo test"]
"#,
        )
        .expect("manifest should parse");
        eval_report_dry_run(&manifest, "run-1", 1).expect("report should be deterministic")
    }

    #[test]
    fn one_case_and_one_run_event_are_built() {
        let report = report();
        let events = build_eval_events(&report).expect("events should serialize");
        let retry = build_eval_events(&report).expect("retry events should serialize");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].hook, "eval_case_scored");
        assert_eq!(events[1].hook, "eval_run_completed");
        assert_eq!(events[0].id, retry[0].id);
        assert_eq!(events[1].id, retry[1].id);
        assert!(events[0]
            .content
            .as_deref()
            .is_some_and(|content| content.contains("\"case_id\":\"case-1\"")));
        assert!(events[1]
            .content
            .as_deref()
            .is_some_and(|content| content.contains("\"run_id\":\"run-1\"")));
    }

    #[test]
    fn budget_exhausted_run_event_is_blocking() {
        let mut report = report();
        report.outcome = Some(crate::runtime::EvalRunOutcome::BudgetExhausted);

        let events = build_eval_events(&report).expect("events should serialize");

        assert_eq!(events[1].decision, Decision::Block);
    }

    #[test]
    fn infrastructure_incomplete_run_event_is_blocking() {
        let mut report = report();
        report.outcome = Some(crate::runtime::EvalRunOutcome::Incomplete);

        let events = build_eval_events(&report).expect("events should serialize");

        assert_eq!(events[1].decision, Decision::Block);
    }

    #[test]
    fn legacy_incomplete_run_without_outcome_is_blocking() {
        let mut report = report();
        report.outcome = None;

        let events = build_eval_events(&report).expect("events should serialize");

        assert_eq!(events[1].decision, Decision::Block);
        assert!(events[1]
            .content
            .as_deref()
            .is_some_and(|content| content.contains("\"outcome\":\"incomplete\"")));
    }

    #[test]
    fn legacy_event_retry_restores_incomplete_outcome() {
        let mut report = report();
        report.outcome = Some(EvalRunOutcome::EventPersistenceFailed);

        let recovered =
            super::super::report_after_event_retry(&report, EvalRunOutcome::EventPersistenceFailed);

        assert_eq!(recovered.outcome, Some(EvalRunOutcome::Incomplete));
    }

    #[tokio::test]
    async fn event_store_failure_is_returned_to_the_eval_executor() {
        let store = EventStore::new_noop_for_tests();

        let error = emit_eval_events(&store, &report())
            .await
            .expect_err("event persistence failure must remain visible");

        assert!(!error.to_string().trim().is_empty());
    }

    #[tokio::test]
    async fn failed_event_report_retries_idempotently_in_postgres() -> anyhow::Result<()> {
        let failed_store = EventStore::new_noop_for_tests();
        let report = report();
        let source = emit_eval_events(&failed_store, &report)
            .await
            .expect_err("fault-injected event persistence must fail");
        let failure = EvalEventPersistenceError::new(source, report);
        assert_eq!(
            failure.report().outcome,
            Some(EvalRunOutcome::IncompleteAndEventPersistenceFailed)
        );

        let Some(database_url) =
            harness_core::config::process_env::var("HARNESS_DATABASE_URL").ok()
        else {
            return Ok(());
        };

        let directory = tempfile::tempdir()?;
        let recovered_store =
            EventStore::new_with_database_url(directory.path(), Some(&database_url)).await?;
        let recovered =
            super::super::retry_eval_report_events(&recovered_store, failure.report()).await?;
        assert_eq!(recovered.outcome, Some(EvalRunOutcome::Incomplete));
        let repeated =
            super::super::retry_eval_report_events(&recovered_store, failure.report()).await?;
        assert_eq!(repeated, recovered);

        let events = recovered_store.query(&EventFilters::default()).await?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.hook == "eval_case_scored")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.hook == "eval_run_completed")
                .count(),
            1
        );
        recovered_store.shutdown().await;
        Ok(())
    }
}
