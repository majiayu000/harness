use super::{round_cost, PriceCatalog, UsageRecord};
use harness_observe::usage::UsageMetrics;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub(super) struct UsageAggregate {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
    pub(super) request_count: u64,
    pub(super) estimated_cost_usd: f64,
    pub(super) missing_price: bool,
}

impl UsageAggregate {
    pub(super) fn add(&mut self, metrics: &UsageMetrics, estimated_cost_usd: Option<f64>) {
        self.input_tokens = self.input_tokens.saturating_add(metrics.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(metrics.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(metrics.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(metrics.cache_creation_input_tokens);
        self.request_count = self.request_count.saturating_add(1);
        match estimated_cost_usd {
            Some(cost) => self.estimated_cost_usd += cost,
            None => self.missing_price = true,
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.request_count = self.request_count.saturating_add(other.request_count);
        self.estimated_cost_usd += other.estimated_cost_usd;
        self.missing_price |= other.missing_price;
    }

    pub(super) fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    pub(super) fn estimated_cost_json(&self, prices_configured: bool) -> Option<f64> {
        if prices_configured && !self.missing_price {
            Some(round_cost(self.estimated_cost_usd))
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct UsageGroup {
    pub(super) name: String,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) request_count: u64,
    pub(super) estimated_cost_usd: Option<f64>,
    pub(super) cost_confidence: &'static str,
}

pub(super) fn aggregate_usage<F>(
    records: &[UsageRecord],
    key_fn: F,
    price_catalog: &PriceCatalog,
) -> Vec<UsageGroup>
where
    F: Fn(&UsageRecord) -> String,
{
    let mut groups: BTreeMap<String, UsageAggregate> = BTreeMap::new();
    for record in records {
        groups
            .entry(key_fn(record))
            .or_default()
            .add(&record.metrics, record.estimated_cost_usd);
    }
    let mut rows = groups
        .into_iter()
        .map(|(name, usage)| usage_group(name, usage, price_catalog.configured()))
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

pub(super) fn total_usage_aggregate(records: &[UsageRecord]) -> UsageAggregate {
    let mut usage = UsageAggregate::default();
    for record in records {
        usage.add(&record.metrics, record.estimated_cost_usd);
    }
    usage
}

pub(super) fn usage_group(
    name: String,
    usage: UsageAggregate,
    prices_configured: bool,
) -> UsageGroup {
    UsageGroup {
        name,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        total_tokens: usage.total_tokens(),
        request_count: usage.request_count,
        estimated_cost_usd: usage.estimated_cost_json(prices_configured),
        cost_confidence: if prices_configured && !usage.missing_price {
            "exact_tokens_estimated_price"
        } else {
            "price_unavailable"
        },
    }
}
