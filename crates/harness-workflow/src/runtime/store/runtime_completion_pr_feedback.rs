use crate::runtime::model::{WorkflowDecision, WorkflowEvent, WorkflowInstance};
use crate::runtime::{
    next_feedback_repair_round, DataProvenance, WorkflowDataWrite, GITHUB_ISSUE_PR_DEFINITION_ID,
    LOCAL_REVIEW_ACTIVITY, LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL, PR_FEEDBACK_DEFINITION_ID,
    PR_FEEDBACK_INSPECT_ACTIVITY, SERVER_PR_SNAPSHOT_ARTIFACT,
};
use serde_json::{json, Value};

pub(super) fn apply_pr_feedback_completion_data_side_effect(
    instance: &mut WorkflowInstance,
    decision: &WorkflowDecision,
    event: &WorkflowEvent,
) -> anyhow::Result<()> {
    if local_review_requests_repair(instance, decision) {
        let blocker_count = local_review_blocker_count(event);
        let next_round = match blocker_count {
            Some(blocker_count) => next_feedback_repair_round(&instance.data, blocker_count)
                .map_err(|stop| {
                    anyhow::anyhow!("local review repair progress was rejected: {stop:?}")
                })?,
            None => {
                let completed_rounds = instance
                    .data
                    .get("feedback_repair_round")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if completed_rounds != 0 {
                    anyhow::bail!(
                        "local review repair progress cannot be measured after a prior repair round"
                    );
                }
                1
            }
        };
        ensure_object_data(instance);
        let mut writes = vec![WorkflowDataWrite::set(
            "feedback_repair_round",
            json!(next_round),
            DataProvenance::Server,
        )];
        if let Some(blocker_count) = blocker_count {
            writes.push(WorkflowDataWrite::set(
                "feedback_repair_blocker_count",
                json!(blocker_count),
                DataProvenance::Server,
            ));
        } else {
            writes.push(WorkflowDataWrite::remove(
                "feedback_repair_blocker_count",
                DataProvenance::Server,
            ));
        }
        return instance.apply_data_writes(writes);
    }
    let child_inspection = instance.definition_id == PR_FEEDBACK_DEFINITION_ID
        && instance.state == "inspecting"
        && decision.observed_state == "inspecting"
        && (matches!(
            decision.next_state.as_str(),
            "feedback_found" | "no_actionable_feedback" | "ready_to_merge"
        ) || (decision.next_state == "blocked"
            && decision.decision.starts_with("block_feedback_repair_")));
    let parent_inspection = instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.state == "awaiting_feedback"
        && decision.observed_state == "awaiting_feedback"
        && (matches!(
            decision.next_state.as_str(),
            "addressing_feedback" | "awaiting_feedback" | "quality_gate_pending"
        ) || (decision.next_state == "blocked"
            && decision.decision.starts_with("block_feedback_repair_")));
    if !child_inspection && !parent_inspection {
        return Ok(());
    }
    let Some(snapshot) = pr_feedback_snapshot_from_completion_event(instance, event) else {
        if parent_inspection && decision.next_state == "addressing_feedback" {
            return apply_snapshotless_parent_repair_progress(instance, event);
        }
        return Ok(());
    };
    let facts_for_hash = crate::runtime::stable_pr_snapshot_fact_hash_input(snapshot);
    let fact_hash = crate::runtime::stable_remote_fact_hash(&facts_for_hash);
    let activity_at = ["updated_at", "updatedAt"].into_iter().find_map(|field| {
        snapshot
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });
    ensure_object_data(instance);
    let mut writes = vec![WorkflowDataWrite::set(
        "remote_fact_hash",
        json!(fact_hash),
        DataProvenance::Server,
    )];
    if parent_inspection {
        apply_parent_inspection_progress(instance, decision, snapshot, &mut writes)?;
        apply_parent_pr_identity(snapshot, &mut writes);
    }
    writes.push(match activity_at {
        Some(activity_at) => WorkflowDataWrite::set(
            "remote_fact_activity_at",
            json!(activity_at),
            DataProvenance::External,
        ),
        None => WorkflowDataWrite::remove("remote_fact_activity_at", DataProvenance::External),
    });
    instance.apply_data_writes(writes)
}

fn local_review_requests_repair(instance: &WorkflowInstance, decision: &WorkflowDecision) -> bool {
    instance.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && instance.state == "local_review_gate"
        && decision.observed_state == "local_review_gate"
        && decision.next_state == "addressing_feedback"
}

fn local_review_blocker_count(event: &WorkflowEvent) -> Option<u64> {
    let result = event.event.get("activity_result")?;
    if result.get("activity").and_then(Value::as_str) != Some(LOCAL_REVIEW_ACTIVITY) {
        return None;
    }
    result
        .get("signals")
        .and_then(Value::as_array)?
        .iter()
        .find(|signal| {
            signal.get("signal_type").and_then(Value::as_str)
                == Some(LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL)
        })
        .and_then(|signal| signal.pointer("/signal/actionable_blocker_count"))
        .and_then(Value::as_u64)
}

fn apply_snapshotless_parent_repair_progress(
    instance: &mut WorkflowInstance,
    event: &WorkflowEvent,
) -> anyhow::Result<()> {
    let blocker_count = pr_feedback_signal_blocker_count(event);
    let next_round = match blocker_count {
        Some(blocker_count) => {
            next_feedback_repair_round(&instance.data, blocker_count).map_err(|stop| {
                anyhow::anyhow!("PR feedback repair progress was rejected: {stop:?}")
            })?
        }
        None => {
            let completed_rounds = instance
                .data
                .get("feedback_repair_round")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if completed_rounds != 0 {
                anyhow::bail!(
                    "PR feedback repair progress cannot be measured after a prior repair round"
                );
            }
            1
        }
    };
    ensure_object_data(instance);
    let mut writes = vec![WorkflowDataWrite::set(
        "feedback_repair_round",
        json!(next_round),
        DataProvenance::Server,
    )];
    if let Some(blocker_count) = blocker_count {
        writes.push(WorkflowDataWrite::set(
            "feedback_repair_blocker_count",
            json!(blocker_count),
            DataProvenance::Server,
        ));
    } else {
        writes.push(WorkflowDataWrite::remove(
            "feedback_repair_blocker_count",
            DataProvenance::Server,
        ));
    }
    instance.apply_data_writes(writes)
}

fn pr_feedback_signal_blocker_count(event: &WorkflowEvent) -> Option<u64> {
    event
        .event
        .pointer("/activity_result/signals")?
        .as_array()?
        .iter()
        .find_map(|signal| {
            signal
                .pointer("/signal/actionable_blocker_count")
                .and_then(Value::as_u64)
        })
}

fn apply_parent_inspection_progress(
    instance: &WorkflowInstance,
    decision: &WorkflowDecision,
    snapshot: &Value,
    writes: &mut Vec<WorkflowDataWrite>,
) -> anyhow::Result<()> {
    if decision.next_state == "addressing_feedback" {
        let blocker_count = snapshot
            .get("actionable_blocker_count")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "server-owned PR feedback snapshot is missing actionable_blocker_count"
                )
            })?;
        let next_round =
            next_feedback_repair_round(&instance.data, blocker_count).map_err(|stop| {
                anyhow::anyhow!("PR feedback repair progress was rejected: {stop:?}")
            })?;
        writes.push(WorkflowDataWrite::set(
            "feedback_repair_round",
            json!(next_round),
            DataProvenance::Server,
        ));
        writes.push(WorkflowDataWrite::set(
            "feedback_repair_blocker_count",
            json!(blocker_count),
            DataProvenance::Server,
        ));
    } else if decision.next_state == "quality_gate_pending"
        || (decision.next_state != "blocked"
            && snapshot
                .get("actionable_blocker_count")
                .and_then(Value::as_u64)
                == Some(0)
            && snapshot
                .get("status_check_rollup_state")
                .and_then(Value::as_str)
                .is_some_and(|state| state.eq_ignore_ascii_case("SUCCESS")))
    {
        writes.push(WorkflowDataWrite::remove(
            "feedback_repair_round",
            DataProvenance::Server,
        ));
        writes.push(WorkflowDataWrite::remove(
            "feedback_repair_blocker_count",
            DataProvenance::Server,
        ));
    }
    Ok(())
}

fn apply_parent_pr_identity(snapshot: &Value, writes: &mut Vec<WorkflowDataWrite>) {
    if snapshot.get("snapshot_source").and_then(Value::as_str) != Some("server_github_graphql") {
        return;
    }
    if let Some(head_sha) = ["head_oid", "head_sha", "headOid", "headSha"]
        .into_iter()
        .find_map(|field| snapshot.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        writes.push(WorkflowDataWrite::set(
            "pr_head_sha",
            json!(head_sha),
            DataProvenance::External,
        ));
    }
    if let Some(pr_url) = ["pr_url", "prUrl", "url"]
        .into_iter()
        .find_map(|field| snapshot.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        writes.push(WorkflowDataWrite::set(
            "pr_url",
            json!(pr_url),
            DataProvenance::External,
        ));
    }
}

fn ensure_object_data(instance: &mut WorkflowInstance) {
    if !instance.data.is_object() {
        instance.replace_classified_data(json!({}), DataProvenance::Server);
    }
}

fn pr_feedback_snapshot_from_completion_event<'a>(
    instance: &WorkflowInstance,
    event: &'a WorkflowEvent,
) -> Option<&'a Value> {
    let result = event.event.get("activity_result")?;
    if !matches!(
        result.get("activity").and_then(Value::as_str),
        Some("sweep_pr_feedback") | Some(PR_FEEDBACK_INSPECT_ACTIVITY)
    ) {
        return None;
    }
    result
        .get("artifacts")
        .and_then(Value::as_array)?
        .iter()
        .filter(|artifact| {
            artifact.get("artifact_type").and_then(Value::as_str)
                == Some(SERVER_PR_SNAPSHOT_ARTIFACT)
        })
        .filter_map(|artifact| artifact.get("artifact"))
        .find(|snapshot| {
            crate::runtime::pr_feedback::server_pr_snapshot_matches_instance(instance, snapshot)
        })
}
