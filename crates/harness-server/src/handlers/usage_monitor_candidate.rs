use super::usage_monitor_aggregate::UsageAggregate;
use super::{RuntimeUsageRow, UsageRecord};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) type CandidateAttributionIndex = BTreeMap<String, CandidateUsageAttribution>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct CandidateUsageAttribution {
    pub candidate_group_id: String,
    pub candidate_id: String,
    pub candidate_index: Option<u32>,
    pub candidate_count: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(super) struct CandidateUsageGroup {
    pub(super) candidate_group_id: String,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) request_count: u64,
    pub(super) estimated_cost_usd: Option<f64>,
    pub(super) cost_confidence: &'static str,
    pub(super) candidates: Vec<CandidateUsageRow>,
}

#[derive(Debug, Serialize)]
pub(super) struct CandidateUsageRow {
    pub(super) candidate_id: String,
    pub(super) candidate_index: Option<u32>,
    pub(super) candidate_count: Option<u32>,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) request_count: u64,
    pub(super) estimated_cost_usd: Option<f64>,
    pub(super) cost_confidence: &'static str,
}

pub(super) fn candidate_attribution_index(rows: &[RuntimeUsageRow]) -> CandidateAttributionIndex {
    let mut index = BTreeMap::new();
    for row in rows {
        let Some(attribution) = candidate_attribution_from_row(row) else {
            continue;
        };
        insert_key(&mut index, row.command_id.as_str(), &attribution);
        insert_key(&mut index, row.runtime_job.id.as_str(), &attribution);
        insert_key(&mut index, attribution.candidate_id.as_str(), &attribution);
        if let Some(task_id) = non_empty_string_field(&row.runtime_job.input, "task_id") {
            insert_key(&mut index, &task_id, &attribution);
        }
        if let Some(task_id) = candidate_workspace_task_id(row, &attribution) {
            insert_key(&mut index, &task_id, &attribution);
        }
    }
    index
}

pub(super) fn usage_record_candidate(
    payload: &Value,
    index: &CandidateAttributionIndex,
) -> Option<CandidateUsageAttribution> {
    direct_candidate_attribution(payload).or_else(|| {
        ["candidate_id", "runtime_job_id", "command_id", "task_id"]
            .into_iter()
            .filter_map(|field| non_empty_string_field(payload, field))
            .find_map(|key| index.get(key.as_str()).cloned())
    })
}

pub(super) fn candidate_usage_groups(
    records: &[UsageRecord],
    prices_configured: bool,
) -> Vec<CandidateUsageGroup> {
    let mut groups: BTreeMap<String, BTreeMap<String, CandidateUsageAccumulator>> = BTreeMap::new();
    for record in records {
        let Some(attribution) = record.candidate.clone() else {
            continue;
        };
        groups
            .entry(attribution.candidate_group_id.clone())
            .or_default()
            .entry(attribution.candidate_id.clone())
            .or_insert_with(|| CandidateUsageAccumulator::new(attribution))
            .usage
            .add(&record.metrics, record.estimated_cost_usd);
    }

    let mut rows = groups
        .into_iter()
        .map(|(candidate_group_id, candidates)| {
            candidate_usage_group(candidate_group_id, candidates, prices_configured)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .total_tokens
            .cmp(&left.total_tokens)
            .then_with(|| left.candidate_group_id.cmp(&right.candidate_group_id))
    });
    rows
}

fn candidate_usage_group(
    candidate_group_id: String,
    candidates: BTreeMap<String, CandidateUsageAccumulator>,
    prices_configured: bool,
) -> CandidateUsageGroup {
    let mut total = UsageAggregate::default();
    let mut candidate_rows = candidates
        .into_values()
        .map(|accumulator| {
            total.merge(&accumulator.usage);
            candidate_usage_row(accumulator, prices_configured)
        })
        .collect::<Vec<_>>();
    candidate_rows.sort_by(|left, right| {
        left.candidate_index
            .cmp(&right.candidate_index)
            .then_with(|| left.candidate_id.cmp(&right.candidate_id))
    });

    CandidateUsageGroup {
        candidate_group_id,
        input_tokens: total.input_tokens,
        output_tokens: total.output_tokens,
        cache_read_input_tokens: total.cache_read_input_tokens,
        cache_creation_input_tokens: total.cache_creation_input_tokens,
        total_tokens: total.total_tokens(),
        request_count: total.request_count,
        estimated_cost_usd: total.estimated_cost_json(prices_configured),
        cost_confidence: if prices_configured && !total.missing_price {
            "exact_tokens_estimated_price"
        } else {
            "price_unavailable"
        },
        candidates: candidate_rows,
    }
}

fn candidate_usage_row(
    accumulator: CandidateUsageAccumulator,
    prices_configured: bool,
) -> CandidateUsageRow {
    CandidateUsageRow {
        candidate_id: accumulator.attribution.candidate_id,
        candidate_index: accumulator.attribution.candidate_index,
        candidate_count: accumulator.attribution.candidate_count,
        input_tokens: accumulator.usage.input_tokens,
        output_tokens: accumulator.usage.output_tokens,
        cache_read_input_tokens: accumulator.usage.cache_read_input_tokens,
        cache_creation_input_tokens: accumulator.usage.cache_creation_input_tokens,
        total_tokens: accumulator.usage.total_tokens(),
        request_count: accumulator.usage.request_count,
        estimated_cost_usd: accumulator.usage.estimated_cost_json(prices_configured),
        cost_confidence: if prices_configured && !accumulator.usage.missing_price {
            "exact_tokens_estimated_price"
        } else {
            "price_unavailable"
        },
    }
}

fn candidate_attribution_from_row(row: &RuntimeUsageRow) -> Option<CandidateUsageAttribution> {
    let candidate = row
        .runtime_job
        .input
        .pointer("/command/candidate")
        .or_else(|| row.command.command.get("candidate"))?;
    let candidate_group_id = non_empty_string_field(candidate, "candidate_group_id")?;
    let candidate_id = non_empty_string_field(candidate, "candidate_id")?;
    Some(CandidateUsageAttribution {
        candidate_group_id,
        candidate_id,
        candidate_index: u32_field(candidate, "candidate_index"),
        candidate_count: u32_field(candidate, "candidate_count"),
    })
}

fn direct_candidate_attribution(payload: &Value) -> Option<CandidateUsageAttribution> {
    let candidate_group_id = non_empty_string_field(payload, "candidate_group_id")?;
    let candidate_id = non_empty_string_field(payload, "candidate_id")?;
    Some(CandidateUsageAttribution {
        candidate_group_id,
        candidate_id,
        candidate_index: u32_field(payload, "candidate_index"),
        candidate_count: u32_field(payload, "candidate_count"),
    })
}

fn candidate_workspace_task_id(
    row: &RuntimeUsageRow,
    attribution: &CandidateUsageAttribution,
) -> Option<String> {
    let issue_number = row.workflow.data.get("issue_number")?.as_u64()?;
    let candidate_index = attribution.candidate_index?;
    Some(format!("issue-{issue_number}-c{candidate_index}"))
}

fn insert_key(
    index: &mut CandidateAttributionIndex,
    key: &str,
    attribution: &CandidateUsageAttribution,
) {
    if !key.trim().is_empty() {
        index.insert(key.to_string(), attribution.clone());
    }
}

fn non_empty_string_field(value: &Value, field: &str) -> Option<String> {
    super::string_field(value, field).filter(|value| !value.trim().is_empty())
}

fn u32_field(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

struct CandidateUsageAccumulator {
    attribution: CandidateUsageAttribution,
    usage: UsageAggregate,
}

impl CandidateUsageAccumulator {
    fn new(attribution: CandidateUsageAttribution) -> Self {
        Self {
            attribution,
            usage: UsageAggregate::default(),
        }
    }
}
