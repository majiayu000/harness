use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::process::Output;
use std::time::Instant;
use tokio::process::Command;

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
struct CcstatsSessionRow {
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
    models: Vec<String>,
    first_timestamp: Option<DateTime<Utc>>,
    last_timestamp: Option<DateTime<Utc>>,
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
    let since = window.since;
    let until = window.now;
    let codex = load_local_usage_source_summary(LOCAL_USAGE_SOURCES[0], since, until);
    let claude = load_local_usage_source_summary(LOCAL_USAGE_SOURCES[1], since, until);
    let (codex, claude) = tokio::join!(codex, claude);
    vec![codex, claude]
}

async fn load_local_usage_source_summary(
    source: LocalUsageSource,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> LocalUsageSourceSummary {
    let result = summarize_source_with_ccstats_cli(source, since, until).await;
    local_usage_source_summary_from_result(source, since, until, result)
}

#[cfg(test)]
fn collect_local_usage_summaries_with<F>(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    mut summarize: F,
) -> Vec<LocalUsageSourceSummary>
where
    F: FnMut(
        LocalUsageSource,
        DateTime<Utc>,
        DateTime<Utc>,
    ) -> Result<(Vec<CcstatsSessionRow>, f64), String>,
{
    LOCAL_USAGE_SOURCES
        .into_iter()
        .map(|source| {
            let result = summarize(source, since, until);
            local_usage_source_summary_from_result(source, since, until, result)
        })
        .collect()
}

async fn summarize_source_with_ccstats_cli(
    source: LocalUsageSource,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<(Vec<CcstatsSessionRow>, f64), String> {
    let started = Instant::now();
    let since_date = since.date_naive().to_string();
    let until_date = until.date_naive().to_string();
    let output = Command::new(CCSTATS_BIN)
        .args([
            "session",
            "--source",
            source.source,
            "--since",
            &since_date,
            "--until",
            &until_date,
            "--json",
            "--offline",
            "--currency",
            "USD",
            "--timezone",
            "UTC",
        ])
        .output()
        .await
        .map_err(|error| format!("failed to run `{CCSTATS_BIN}`: {error}"))?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    if !output.status.success() {
        return Err(ccstats_failure_message(&output));
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|error| format!("ccstats stdout was not UTF-8: {error}"))?;
    parse_ccstats_session_rows(stdout)
        .map(|rows| (rows, elapsed_ms))
        .map_err(|error| format!("ccstats JSON output was invalid: {error}"))
}

fn local_usage_source_summary_from_result(
    source: LocalUsageSource,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    result: Result<(Vec<CcstatsSessionRow>, f64), String>,
) -> LocalUsageSourceSummary {
    match result {
        Ok((rows, elapsed_ms)) => available_summary(source, since, until, rows, elapsed_ms),
        Err(error) => unavailable_summary(source, since, until, 0.0, error),
    }
}

fn available_summary(
    source: LocalUsageSource,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    rows: Vec<CcstatsSessionRow>,
    elapsed_ms: f64,
) -> LocalUsageSourceSummary {
    let mut total = LocalUsageAccumulator::default();
    let mut model_names = BTreeSet::new();
    let mut period_count = 0;
    for row in rows {
        match row.overlaps_window(since, until) {
            Ok(true) => {
                total.add_session(&row);
                period_count += 1;
                model_names.extend(
                    row.models
                        .iter()
                        .map(|model| model.trim())
                        .filter(|model| !model.is_empty())
                        .map(ToString::to_string),
                );
            }
            Ok(false) => {}
            Err(error) => return unavailable_summary(source, since, until, elapsed_ms, error),
        }
    }

    LocalUsageSourceSummary {
        source: source.source,
        display_name: source.display_name,
        attribution: "global_local_source_logs",
        status: "available",
        since: format_window_timestamp(since),
        until: format_window_timestamp(until),
        currency: "USD",
        estimated_cost_usd: total.cost_json(),
        input_tokens: total.input_tokens,
        output_tokens: total.output_tokens,
        reasoning_tokens: total.reasoning_tokens,
        cache_read_input_tokens: total.cache_read_input_tokens,
        cache_creation_input_tokens: total.cache_creation_input_tokens,
        total_tokens: total.total_tokens,
        period_count,
        model_count: model_names.len(),
        models: Vec::new(),
        cost_confidence: if total.has_cost {
            "ccstats_session_window_estimated_price"
        } else {
            "ccstats_session_window_price_unavailable"
        },
        elapsed_ms,
        error: None,
    }
}

fn unavailable_summary(
    source: LocalUsageSource,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    elapsed_ms: f64,
    error: String,
) -> LocalUsageSourceSummary {
    LocalUsageSourceSummary {
        source: source.source,
        display_name: source.display_name,
        attribution: "global_local_source_logs",
        status: "unavailable",
        since: format_window_timestamp(since),
        until: format_window_timestamp(until),
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

fn parse_ccstats_session_rows(stdout: &str) -> Result<Vec<CcstatsSessionRow>, serde_json::Error> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() || trimmed.contains(" session data found in the selected date range") {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<CcstatsSessionRow>>(trimmed)
}

fn format_window_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

impl CcstatsSessionRow {
    fn overlaps_window(&self, since: DateTime<Utc>, until: DateTime<Utc>) -> Result<bool, String> {
        let Some(first_timestamp) = self.first_timestamp else {
            return Err("ccstats session row was missing first_timestamp".to_string());
        };
        let Some(last_timestamp) = self.last_timestamp else {
            return Err("ccstats session row was missing last_timestamp".to_string());
        };
        if last_timestamp < first_timestamp {
            return Err(
                "ccstats session row had last_timestamp before first_timestamp".to_string(),
            );
        }
        Ok(last_timestamp >= since && first_timestamp <= until)
    }
}

impl LocalUsageAccumulator {
    fn add_session(&mut self, row: &CcstatsSessionRow) {
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
        let since = utc_timestamp("2026-06-03T14:00:00Z");
        let until = utc_timestamp("2026-06-04T14:00:00Z");

        let summaries = collect_local_usage_summaries_with(since, until, |source, _, _| {
            if source.source == "codex" {
                Ok((vec![fake_session_row()], 7.5))
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
        assert!(codex.models.is_empty());
        assert_eq!(codex.elapsed_ms, 7.5);
        assert_eq!(codex.since, "2026-06-03T14:00:00Z");
        assert_eq!(codex.until, "2026-06-04T14:00:00Z");

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
    fn available_summary_filters_sessions_to_requested_window() {
        let since = utc_timestamp("2026-06-03T14:00:00Z");
        let until = utc_timestamp("2026-06-04T14:00:00Z");
        let mut second = fake_session_row();
        second.input_tokens = 20;
        second.total_tokens = 30;
        second.cost = Some(0.5);
        second.models = vec!["gpt-5.5".to_string()];
        second.first_timestamp = Some(utc_timestamp("2026-06-04T12:00:00Z"));
        second.last_timestamp = Some(utc_timestamp("2026-06-04T12:10:00Z"));
        let mut outside_window = fake_session_row();
        outside_window.input_tokens = 100;
        outside_window.total_tokens = 100;
        outside_window.cost = Some(10.0);
        outside_window.first_timestamp = Some(utc_timestamp("2026-06-03T08:00:00Z"));
        outside_window.last_timestamp = Some(utc_timestamp("2026-06-03T08:10:00Z"));

        let summary = available_summary(
            LOCAL_USAGE_SOURCES[0],
            since,
            until,
            vec![fake_session_row(), second, outside_window],
            12.0,
        );

        assert_eq!(summary.period_count, 2);
        assert_eq!(summary.input_tokens, 30);
        assert_eq!(summary.total_tokens, 54);
        assert_eq!(summary.estimated_cost_usd, Some(0.6235));
        assert_eq!(summary.model_count, 2);
        assert!(summary.models.is_empty());
    }

    #[test]
    fn parse_ccstats_session_rows_treats_no_data_message_as_empty() {
        let rows = match parse_ccstats_session_rows(
            "No OpenAI Codex session data found in the selected date range.",
        ) {
            Ok(rows) => rows,
            Err(error) => panic!("no-data ccstats output should parse as empty: {error}"),
        };

        assert!(rows.is_empty());
    }

    fn fake_session_row() -> CcstatsSessionRow {
        CcstatsSessionRow {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 2,
            cache_read_tokens: 3,
            cache_creation_tokens: 4,
            total_tokens: 24,
            cost: Some(0.123456),
            models: vec!["gpt-5".to_string()],
            first_timestamp: Some(utc_timestamp("2026-06-04T08:00:00Z")),
            last_timestamp: Some(utc_timestamp("2026-06-04T08:10:00Z")),
        }
    }

    fn utc_timestamp(raw: &str) -> DateTime<Utc> {
        match DateTime::parse_from_rfc3339(raw) {
            Ok(timestamp) => timestamp.with_timezone(&Utc),
            Err(error) => panic!("valid RFC3339 timestamp: {error}"),
        }
    }
}
