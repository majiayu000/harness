use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::{sync::OnceLock, time::Instant};
use tokio::{
    process::Command,
    sync::Mutex,
    time::{timeout, Duration},
};

use super::round_cost;

const CCSTATS_TIMEOUT_SECS: u64 = 15;
const CCSTATS_CACHE_TTL_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize)]
pub(super) struct CcstatsLocalSource {
    pub source: &'static str,
    pub display_name: &'static str,
    pub status: &'static str,
    pub message: Option<String>,
    pub range: &'static str,
    pub since: String,
    pub until: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub models: Vec<CcstatsLocalModel>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct CcstatsLocalModel {
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct CcstatsDailyRow {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    breakdown: Vec<CcstatsModelRow>,
}

#[derive(Debug, Deserialize)]
struct CcstatsModelRow {
    model: String,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cost: Option<f64>,
}

pub(super) async fn load_ccstats_local_sources(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Vec<CcstatsLocalSource> {
    let since = since.with_timezone(&Local).date_naive().to_string();
    let until = until.with_timezone(&Local).date_naive().to_string();

    let mut cache = ccstats_local_sources_cache().lock().await;
    if let Some(sources) = cache
        .as_ref()
        .and_then(|entry| cached_local_sources(entry, &since, &until, Instant::now()))
    {
        return sources;
    }

    let sources = load_ccstats_local_sources_uncached(&since, &until).await;
    *cache = Some(CcstatsLocalSourcesCacheEntry {
        since,
        until,
        loaded_at: Instant::now(),
        sources: sources.clone(),
    });
    sources
}

async fn load_ccstats_local_sources_uncached(since: &str, until: &str) -> Vec<CcstatsLocalSource> {
    let codex = load_ccstats_source("codex", "Codex", since, until);
    let claude = load_ccstats_source("claude", "Claude", since, until);
    let (codex, claude) = tokio::join!(codex, claude);
    vec![codex, claude]
}

#[derive(Debug)]
struct CcstatsLocalSourcesCacheEntry {
    since: String,
    until: String,
    loaded_at: Instant,
    sources: Vec<CcstatsLocalSource>,
}

static CCSTATS_LOCAL_SOURCES_CACHE: OnceLock<Mutex<Option<CcstatsLocalSourcesCacheEntry>>> =
    OnceLock::new();

fn ccstats_local_sources_cache() -> &'static Mutex<Option<CcstatsLocalSourcesCacheEntry>> {
    CCSTATS_LOCAL_SOURCES_CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_local_sources(
    entry: &CcstatsLocalSourcesCacheEntry,
    since: &str,
    until: &str,
    now: Instant,
) -> Option<Vec<CcstatsLocalSource>> {
    if entry.since == since
        && entry.until == until
        && now.saturating_duration_since(entry.loaded_at)
            < Duration::from_secs(CCSTATS_CACHE_TTL_SECS)
    {
        Some(entry.sources.clone())
    } else {
        None
    }
}

pub(super) fn any_ccstats_source_available(sources: &[CcstatsLocalSource]) -> bool {
    sources.iter().any(|source| source.status == "available")
}

async fn load_ccstats_source(
    source: &'static str,
    display_name: &'static str,
    since: &str,
    until: &str,
) -> CcstatsLocalSource {
    match run_ccstats(source, since, until).await {
        Ok(rows) => source_from_rows(source, display_name, since, until, rows),
        Err(message) => unavailable_source(source, display_name, since, until, message),
    }
}

async fn run_ccstats(
    source: &str,
    since: &str,
    until: &str,
) -> Result<Vec<CcstatsDailyRow>, String> {
    let bin = std::env::var("HARNESS_USAGE_CCSTATS_BIN").unwrap_or_else(|_| "ccstats".to_string());
    let mut command = Command::new(bin);
    command
        .arg("daily")
        .arg("--source")
        .arg(source)
        .arg("--since")
        .arg(since)
        .arg("--until")
        .arg(until)
        .arg("--json")
        .arg("--breakdown")
        .arg("--offline")
        .kill_on_drop(true);

    let output = timeout(Duration::from_secs(CCSTATS_TIMEOUT_SECS), command.output())
        .await
        .map_err(|_| "ccstats command timed out".to_string())?
        .map_err(|error| format!("failed to run ccstats: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("ccstats exited with status {}", output.status)
        } else {
            stderr
        });
    }

    serde_json::from_slice::<Vec<CcstatsDailyRow>>(&output.stdout)
        .map_err(|error| format!("failed to parse ccstats JSON: {error}"))
}

fn source_from_rows(
    source: &'static str,
    display_name: &'static str,
    since: &str,
    until: &str,
    rows: Vec<CcstatsDailyRow>,
) -> CcstatsLocalSource {
    let mut aggregate = CcstatsSourceAggregate::default();
    for row in rows {
        aggregate.add_row(row);
    }

    let mut models = aggregate
        .models
        .into_values()
        .map(|mut model| {
            model.estimated_cost_usd = model.estimated_cost_usd.map(round_cost);
            model
        })
        .collect::<Vec<_>>();
    models.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.model.cmp(&b.model))
    });

    CcstatsLocalSource {
        source,
        display_name,
        status: "available",
        message: None,
        range: "local_date_range",
        since: since.to_string(),
        until: until.to_string(),
        input_tokens: aggregate.input_tokens,
        output_tokens: aggregate.output_tokens,
        reasoning_tokens: aggregate.reasoning_tokens,
        cache_creation_tokens: aggregate.cache_creation_tokens,
        cache_read_tokens: aggregate.cache_read_tokens,
        total_tokens: aggregate.total_tokens,
        estimated_cost_usd: aggregate.estimated_cost_usd.map(round_cost),
        models,
    }
}

fn unavailable_source(
    source: &'static str,
    display_name: &'static str,
    since: &str,
    until: &str,
    message: String,
) -> CcstatsLocalSource {
    CcstatsLocalSource {
        source,
        display_name,
        status: "unavailable",
        message: Some(message),
        range: "local_date_range",
        since: since.to_string(),
        until: until.to_string(),
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        total_tokens: 0,
        estimated_cost_usd: None,
        models: Vec::new(),
    }
}

#[derive(Debug, Default)]
struct CcstatsSourceAggregate {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    total_tokens: u64,
    estimated_cost_usd: Option<f64>,
    models: std::collections::BTreeMap<String, CcstatsLocalModel>,
}

impl CcstatsSourceAggregate {
    fn add_row(&mut self, row: CcstatsDailyRow) {
        self.input_tokens = self.input_tokens.saturating_add(row.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(row.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(row.reasoning_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(row.cache_creation_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(row.cache_read_tokens);
        self.total_tokens = self.total_tokens.saturating_add(row.total_tokens);
        self.estimated_cost_usd = add_optional_cost(self.estimated_cost_usd, row.cost);

        for model in row.breakdown {
            let entry =
                self.models
                    .entry(model.model.clone())
                    .or_insert_with(|| CcstatsLocalModel {
                        model: model.model.clone(),
                        ..CcstatsLocalModel::default()
                    });
            entry.input_tokens = entry.input_tokens.saturating_add(model.input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(model.output_tokens);
            entry.reasoning_tokens = entry
                .reasoning_tokens
                .saturating_add(model.reasoning_tokens);
            entry.cache_creation_tokens = entry
                .cache_creation_tokens
                .saturating_add(model.cache_creation_tokens);
            entry.cache_read_tokens = entry
                .cache_read_tokens
                .saturating_add(model.cache_read_tokens);
            entry.total_tokens = entry.total_tokens.saturating_add(model.total_tokens);
            entry.estimated_cost_usd = add_optional_cost(entry.estimated_cost_usd, model.cost);
        }
    }
}

fn add_optional_cost(current: Option<f64>, next: Option<f64>) -> Option<f64> {
    match (current, next) {
        (Some(current), Some(next)) => Some(current + next),
        (Some(current), None) => Some(current),
        (None, Some(next)) => Some(next),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_from_rows_aggregates_daily_rows_and_models() {
        let rows = vec![
            CcstatsDailyRow {
                input_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: 30,
                cache_creation_tokens: 40,
                cache_read_tokens: 50,
                total_tokens: 150,
                cost: Some(0.11111),
                breakdown: vec![CcstatsModelRow {
                    model: "gpt-5.5".to_string(),
                    input_tokens: 10,
                    output_tokens: 20,
                    reasoning_tokens: 30,
                    cache_creation_tokens: 40,
                    cache_read_tokens: 50,
                    total_tokens: 150,
                    cost: Some(0.11111),
                }],
            },
            CcstatsDailyRow {
                input_tokens: 1,
                output_tokens: 2,
                reasoning_tokens: 3,
                cache_creation_tokens: 4,
                cache_read_tokens: 5,
                total_tokens: 15,
                cost: Some(0.22222),
                breakdown: vec![CcstatsModelRow {
                    model: "gpt-5.5".to_string(),
                    input_tokens: 1,
                    output_tokens: 2,
                    reasoning_tokens: 3,
                    cache_creation_tokens: 4,
                    cache_read_tokens: 5,
                    total_tokens: 15,
                    cost: Some(0.22222),
                }],
            },
        ];

        let source = source_from_rows("codex", "Codex", "2026-06-04", "2026-06-04", rows);

        assert_eq!(source.status, "available");
        assert_eq!(source.total_tokens, 165);
        assert_eq!(source.estimated_cost_usd, Some(0.3333));
        assert_eq!(source.models.len(), 1);
        assert_eq!(source.models[0].total_tokens, 165);
        assert_eq!(source.models[0].estimated_cost_usd, Some(0.3333));
    }

    #[test]
    fn unavailable_source_keeps_error_message_without_cost() {
        let source = unavailable_source(
            "claude",
            "Claude",
            "2026-06-04",
            "2026-06-04",
            "ccstats missing".to_string(),
        );

        assert_eq!(source.status, "unavailable");
        assert_eq!(source.estimated_cost_usd, None);
        assert_eq!(source.message.as_deref(), Some("ccstats missing"));
    }

    #[test]
    fn cached_local_sources_returns_fresh_matching_range() {
        let now = Instant::now();
        let entry = CcstatsLocalSourcesCacheEntry {
            since: "2026-06-04".to_string(),
            until: "2026-06-05".to_string(),
            loaded_at: now - Duration::from_secs(CCSTATS_CACHE_TTL_SECS - 1),
            sources: vec![unavailable_source(
                "codex",
                "Codex",
                "2026-06-04",
                "2026-06-05",
                "ccstats missing".to_string(),
            )],
        };

        let Some(sources) = cached_local_sources(&entry, "2026-06-04", "2026-06-05", now) else {
            panic!("fresh matching range should be cached");
        };

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source, "codex");
    }

    #[test]
    fn cached_local_sources_misses_expired_or_different_range() {
        let now = Instant::now();
        let entry = CcstatsLocalSourcesCacheEntry {
            since: "2026-06-04".to_string(),
            until: "2026-06-05".to_string(),
            loaded_at: now - Duration::from_secs(CCSTATS_CACHE_TTL_SECS),
            sources: vec![unavailable_source(
                "codex",
                "Codex",
                "2026-06-04",
                "2026-06-05",
                "ccstats missing".to_string(),
            )],
        };

        assert!(cached_local_sources(&entry, "2026-06-04", "2026-06-05", now).is_none());
        assert!(cached_local_sources(&entry, "2026-06-03", "2026-06-05", now).is_none());
    }
}
