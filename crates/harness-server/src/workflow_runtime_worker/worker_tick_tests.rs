use super::*;
use serde_json::json;

#[test]
fn runtime_job_worker_tick_classifies_every_job_status() {
    let mut job = RuntimeJob::pending(
        "command-1",
        harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    );

    job.status = RuntimeJobStatus::Pending;
    assert_eq!(
        RuntimeJobWorkerTick::from_completed_job(Some(job.clone())),
        RuntimeJobWorkerTick::default()
    );

    job.status = RuntimeJobStatus::Running;
    assert_eq!(
        RuntimeJobWorkerTick::from_completed_job(Some(job.clone())),
        RuntimeJobWorkerTick::default()
    );

    job.status = RuntimeJobStatus::Succeeded;
    assert_eq!(
        RuntimeJobWorkerTick::from_completed_job(Some(job.clone())),
        RuntimeJobWorkerTick {
            succeeded: 1,
            ..RuntimeJobWorkerTick::default()
        }
    );

    job.status = RuntimeJobStatus::Failed;
    assert_eq!(
        RuntimeJobWorkerTick::from_completed_job(Some(job.clone())),
        RuntimeJobWorkerTick {
            failed: 1,
            ..RuntimeJobWorkerTick::default()
        }
    );

    job.status = RuntimeJobStatus::Cancelled;
    assert_eq!(
        RuntimeJobWorkerTick::from_completed_job(Some(job)),
        RuntimeJobWorkerTick {
            cancelled: 1,
            ..RuntimeJobWorkerTick::default()
        }
    );
    assert_eq!(
        RuntimeJobWorkerTick::from_completed_job(None),
        RuntimeJobWorkerTick {
            idle: true,
            ..RuntimeJobWorkerTick::default()
        }
    );
}
