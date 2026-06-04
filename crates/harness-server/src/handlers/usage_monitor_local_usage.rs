use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::{Command, Output};
use std::time::Instant;

use super::{round_cost, UsageWindow};

const CCSTATS_BIN: &str = "ccstats";
const LOCAL_USAGE_SOURCES: [LocalUsageSource; 2] = [
    LocalUsageSource {
        source: "codex",
        display_name: "OpenAI Codex",
    },
    LocalUsageSource {
        source: "claude",
        display_name: "Claude Code",
    },
];

#[derive(Debug, Clone, Copy)]
struct LocalUsageSource {
    source: &'static str,
    display_name: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct LocalUsageSourceSummary {
    source: &'static str,
    display_name: &'static str,
    attribution: &'static str,
    status: &'static str,
    since: String,
    until: String,
    currency: &'static str,
    estimated_cost_usd: Option<f64>,
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    total_tokens: u64,
    period_count: u64,
    model_count: usize,
    models: Vec<LocalUsageModelSummary>,
    cost_confidence: &'static str,
    elapsed_ms: f64,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct LocalUsageModelSummary {
    model: String,
    estimated_cost_usd: Option<f64>,
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CcstatsPeriodRow {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    reasoning_tokens: i64,
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_creation_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    cost: Option<f64>,
    #[serde(default)]
    breakdown: Vec<CcstatsModelRow>,
}

#[derive(Debug, Deserialize)]
struct CcstatsModelRow {
    model: String,
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    reasoning_tokens: i64,
    #[serde(default)]
    cache_read_tokens: i64,
    #[serde(default)]
    cache_creation_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    cost: Option<f64>,
}

#[derive(Debug, Default)]
struct LocalUsageAccumulator {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    total_tokens: u64,
    estimated_cost_usd: f64,
    has_cost: bool,
}

pub(super) async fn load_local_usage_summaries(
    window: UsageWindow,
) -> Vec<LocalUsageSourceSummary> {
    let since = window.since.date_naive();
    let until = window.now.date_naive();
    match tokio::task::spawn_blocking(move || {
        collect_local_usage_summaries_with(since, until, summarize_source_with_ccstats_cli)
    })
    .await
    {
        Ok(summaries) => summaries,
        Err(error) => LOCAL_USAGE_SOURCES
            .into_iter()
            .map(|source| {
                unavailable_summary(
                    source,
                    since,
                    until,
                    0.0,
                    format!("ccstats worker failed before summarizing local usage: {error}"),
                )
            })
            .collect(),
    }
}

fn collect_local_usage_summaries_with<F>(
    since: NaiveDate,
    until: NaiveDate,
    mut summarize: F,
) -> Vec<LocalUsageSourceSummary>
where
    F: FnMut(
        LocalUsageSource,
        NaiveDate,
        NaiveDate,
    ) -> Result<(Vec<CcstatsPeriodRow>, f64), String>,
{
    LOCAL_USAGE_SOURCES
        .into_iter()
        .map(|source| {
            let result = summarize(source, since, until);
            local_usage_source_summary_from_result(source, since, until, result)
        })
        .collect()
}

fn summarize_source_with_ccstats_cli(
    source: LocalUsageSource,
    since: NaiveDate,
    until: NaiveDate,
) -> Result<(Vec<CcstatsPeriodRow>, f64), String> {
    let started = Instant::now();
    let output = Command::new(CCSTATS_BIN)
        .args([
            "daily",
            "--source",
            source.source,
            "--since",
            &since.to_string(),
            "--until",
            &until.to_string(),
            "--json",
            "--breakdown",
            "--offline",
            "--currency",
            "USD",
        ])
        .output()
        .map_err(|error| format!("failed to run `{CCSTATS_BIN}`: {error}"))?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    if !output.status.success() {
        return Err(ccstats_failure_message(&output));
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|error| format!("ccstats stdout was not UTF-8: {error}"))?;
    serde_json::from_str::<Vec<CcstatsPeriodRow>>(stdout.trim())
        .map(|rows| (rows, elapsed_ms))
        .map_err(|error| format!("ccstats JSON output was invalid: {error}"))
}

fn local_usage_source_summary_from_result(
    source: LocalUsageSource,
    since: NaiveDate,
    until: NaiveDate,
    result: Result<(Vec<CcstatsPeriodRow>, f64), String>,
) -> LocalUsageSourceSummary {
    match result {
        Ok((rows, elapsed_ms)) => available_summary(source, since, until, rows, elapsed_ms),
        Err(error) => unavailable_summary(source, since, until, 0.0, error),
    }
}

fn available_summary(
    source: LocalUsageSource,
    since: NaiveDate,
    until: NaiveDate,
    rows: Vec<CcstatsPeriodRow>,
    elapsed_ms: f64,
) -> LocalUsageSourceSummary {
    let mut total = LocalUsageAccumulator::default();
    let mut models: BTreeMap<String, LocalUsageAccumulator> = BTreeMap::new();
    for row in &rows {
        total.add_period(row);
        for model in &row.breakdown {
            models
                .entry(model.model.clone())
                .or_default()
                .add_model(model);
        }
    }
    let mut models = models
        .into_iter()
        .map(|(model, usage)| local_usage_model_summary(model, usage))
        .collect::<Vec<_>>();
    models.sort_by(|a, b| {
        b.estimated_cost_usd
            .partial_cmp(&a.estimated_cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.total_tokens.cmp(&a.total_tokens))
            .then_with(|| a.model.cmp(&b.model))
    });

    LocalUsageSourceSummary {
        source: source.source,
        display_name: source.display_name,
        attribution: "global_local_source_logs",
        status: "available",
        since: since.to_string(),
        until: until.to_string(),
        currency: "USD",
        estimated_cost_usd: total.cost_json(),
        input_tokens: total.input_tokens,
        output_tokens: total.output_tokens,
        reasoning_tokens: total.reasoning_tokens,
        cache_read_input_tokens: total.cache_read_input_tokens,
        cache_creation_input_tokens: total.cache_creation_input_tokens,
        total_tokens: total.total_tokens,
        period_count: rows.len() as u64,
        model_count: models.len(),
        models,
        cost_confidence: if total.has_cost {
            "ccstats_cli_estimated_price"
        } else {
            "ccstats_cli_price_unavailable"
        },
        elapsed_ms,
        error: None,
    }
}

fn unavailable_summary(
    source: LocalUsageSource,
    since: NaiveDate,
    until: NaiveDate,
    elapsed_ms: f64,
    error: String,
) -> LocalUsageSourceSummary {
    LocalUsageSourceSummary {
        source: source.source,
        display_name: source.display_name,
        attribution: "global_local_source_logs",
        status: "unavailable",
        since: since.to_string(),
        until: until.to_string(),
        currency: "USD",
        estimated_cost_usd: None,
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        total_tokens: 0,
        period_count: 0,
        model_count: 0,
        models: Vec::new(),
        cost_confidence: "ccstats_cli_unavailable",
        elapsed_ms,
        error: Some(error),
    }
}

fn local_usage_model_summary(
    model: String,
    usage: LocalUsageAccumulator,
) -> LocalUsageModelSummary {
    LocalUsageModelSummary {
        model,
        estimated_cost_usd: usage.cost_json(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        total_tokens: usage.total_tokens,
    }
}

impl LocalUsageAccumulator {
    fn add_period(&mut self, row: &CcstatsPeriodRow) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(nonnegative_tokens(row.input_tokens));
        self.output_tokens = self
            .output_tokens
            .saturating_add(nonnegative_tokens(row.output_tokens));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(nonnegative_tokens(row.reasoning_tokens));
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(nonnegative_tokens(row.cache_read_tokens));
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(nonnegative_tokens(row.cache_creation_tokens));
        self.total_tokens = self
            .total_tokens
            .saturating_add(nonnegative_tokens(row.total_tokens));
        self.add_cost(row.cost);
    }

    fn add_model(&mut self, row: &CcstatsModelRow) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(nonnegative_tokens(row.input_tokens));
        self.output_tokens = self
            .output_tokens
            .saturating_add(nonnegative_tokens(row.output_tokens));
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(nonnegative_tokens(row.reasoning_tokens));
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(nonnegative_tokens(row.cache_read_tokens));
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(nonnegative_tokens(row.cache_creation_tokens));
        self.total_tokens = self
            .total_tokens
            .saturating_add(nonnegative_tokens(row.total_tokens));
        self.add_cost(row.cost);
    }

    fn add_cost(&mut self, cost: Option<f64>) {
        if let Some(cost) = cost.filter(|cost| cost.is_finite()) {
            self.estimated_cost_usd += cost;
            self.has_cost = true;
        }
    }

    fn cost_json(&self) -> Option<f64> {
        self.has_cost.then_some(round_cost(self.estimated_cost_usd))
    }
}

fn ccstats_failure_message(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout.as_ref()
    } else {
        stderr.as_ref()
    };
    let single_line = message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if single_line.is_empty() {
        format!("ccstats exited with status {}", output.status)
    } else {
        truncate_message(&single_line, 500)
    }
}

fn truncate_message(message: &str, max_len: usize) -> String {
    if message.chars().count() <= max_len {
        return message.to_string();
    }
    format!("{}...", message.chars().take(max_len).collect::<String>())
}

fn nonnegative_tokens(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_local_usage_summaries_shapes_success_and_failure() {
        let since = NaiveDate::from_ymd_opt(2026, 6, 3).unwrap();
        let until = NaiveDate::from_ymd_opt(2026, 6, 4).unwrap();

        let summaries = collect_local_usage_summaries_with(since, until, |source, _, _| {
            if source.source == "codex" {
                Ok((vec![fake_period_row()], 7.5))
            } else {
                Err("local logs are unreadable".to_string())
            }
        });

        assert_eq!(summaries.len(), 2);
        let codex = summaries
            .iter()
            .find(|summary| summary.source == "codex")
            .expect("codex summary");
        assert_eq!(codex.status, "available");
        assert_eq!(codex.attribution, "global_local_source_logs");
        assert_eq!(codex.estimated_cost_usd, Some(0.1235));
        assert_eq!(codex.input_tokens, 10);
        assert_eq!(codex.reasoning_tokens, 2);
        assert_eq!(codex.cache_read_input_tokens, 3);
        assert_eq!(codex.cache_creation_input_tokens, 4);
        assert_eq!(codex.total_tokens, 24);
        assert_eq!(codex.period_count, 1);
        assert_eq!(codex.model_count, 1);
        assert_eq!(codex.models[0].model, "gpt-5");
        assert_eq!(codex.elapsed_ms, 7.5);

        let claude = summaries
            .iter()
            .find(|summary| summary.source == "claude")
            .expect("claude summary");
        assert_eq!(claude.status, "unavailable");
        assert_eq!(claude.display_name, "Claude Code");
        assert_eq!(claude.total_tokens, 0);
        assert!(claude
            .error
            .as_deref()
            .is_some_and(|error| error.contains("local logs are unreadable")));
    }

    #[test]
    fn available_summary_aggregates_models_across_periods() {
        let since = NaiveDate::from_ymd_opt(2026, 6, 3).unwrap();
        let until = NaiveDate::from_ymd_opt(2026, 6, 4).unwrap();
        let mut second = fake_period_row();
        second.input_tokens = 20;
        second.total_tokens = 30;
        second.cost = Some(0.5);
        second.breakdown[0].input_tokens = 20;
        second.breakdown[0].total_tokens = 30;
        second.breakdown[0].cost = Some(0.5);

        let summary = available_summary(
            LOCAL_USAGE_SOURCES[0],
            since,
            until,
            vec![fake_period_row(), second],
            12.0,
        );

        assert_eq!(summary.period_count, 2);
        assert_eq!(summary.input_tokens, 30);
        assert_eq!(summary.total_tokens, 54);
        assert_eq!(summary.estimated_cost_usd, Some(0.6235));
        assert_eq!(summary.models.len(), 1);
        assert_eq!(summary.models[0].input_tokens, 30);
        assert_eq!(summary.models[0].estimated_cost_usd, Some(0.6235));
    }

    fn fake_period_row() -> CcstatsPeriodRow {
        CcstatsPeriodRow {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 2,
            cache_read_tokens: 3,
            cache_creation_tokens: 4,
            total_tokens: 24,
            cost: Some(0.123456),
            breakdown: vec![CcstatsModelRow {
                model: "gpt-5".to_string(),
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: 2,
                cache_read_tokens: 3,
                cache_creation_tokens: 4,
                total_tokens: 24,
                cost: Some(0.123456),
            }],
        }
    }
}
