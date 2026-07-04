use super::super::state::{RoundResult, TaskState};
use super::super::types::TaskStatus;

fn subtask_succeeded(result: &crate::parallel_dispatch::SubtaskResult) -> bool {
    result
        .response
        .as_ref()
        .is_some_and(|resp| !resp.output.trim().is_empty())
}

fn subtask_round_detail(result: &crate::parallel_dispatch::SubtaskResult) -> Option<String> {
    match (&result.response, &result.error) {
        (Some(resp), _) if !resp.output.trim().is_empty() => Some(resp.output.clone()),
        (Some(_), _) => Some("agent returned empty output".to_string()),
        (None, Some(error)) => Some(error.clone()),
        (None, None) => None,
    }
}

pub(crate) fn record_parallel_subtask_results(
    state: &mut TaskState,
    run_result: &crate::parallel_dispatch::ParallelRunResult,
) {
    let succeeded = run_result.results.iter().all(subtask_succeeded);
    for result in &run_result.results {
        state.rounds.push(RoundResult::new(
            (result.index as u32).saturating_add(1),
            format!("parallel_subtask_{}", result.index),
            if subtask_succeeded(result) {
                "success"
            } else {
                "failed"
            },
            subtask_round_detail(result),
            None,
            None,
        ));
    }
    if succeeded {
        state.status = TaskStatus::Done;
        state.scheduler.mark_terminal(&TaskStatus::Done);
    } else {
        let failed_count = run_result
            .results
            .iter()
            .filter(|result| !subtask_succeeded(result))
            .count();
        let total_count = run_result.results.len();
        state.status = TaskStatus::Failed;
        state.scheduler.mark_terminal(&TaskStatus::Failed);
        state.error = Some(if run_result.is_sequential {
            format!(
                "{failed_count}/{total_count} sequential subtasks failed; remaining steps were skipped"
            )
        } else {
            format!("{failed_count}/{total_count} parallel subtasks failed")
        });
    }
}
