use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::postgres::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy)]
pub(crate) struct PostgresCatalogThresholds {
    pub(crate) absolute_schema_threshold: u64,
    pub(crate) relative_schema_multiplier: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PostgresCatalogState {
    Available,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct PostgresCatalogCensus {
    pub(crate) state: PostgresCatalogState,
    pub(crate) schema_count: Option<u64>,
    pub(crate) catalog_object_count: Option<u64>,
    pub(crate) database_size_bytes: Option<u64>,
    pub(crate) sampled_at: Option<DateTime<Utc>>,
    pub(crate) startup_schema_count: Option<u64>,
    pub(crate) absolute_schema_threshold: u64,
    pub(crate) relative_schema_multiplier: f64,
    pub(crate) threshold_breached: bool,
    pub(crate) breach_reasons: Vec<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug)]
struct CatalogMonitorState {
    cached: PostgresCatalogCensus,
    last_refresh_at: Option<Instant>,
    startup_schema_count: Option<u64>,
}

#[derive(Debug)]
pub struct PostgresCatalogMonitor {
    pool: Option<PgPool>,
    thresholds: PostgresCatalogThresholds,
    refresh_interval: Duration,
    state: Mutex<CatalogMonitorState>,
}

#[derive(Debug)]
struct RawCatalogCensus {
    schema_count: u64,
    catalog_object_count: u64,
    database_size_bytes: u64,
    sampled_at: DateTime<Utc>,
}

impl PostgresCatalogThresholds {
    pub(crate) fn from_server(_config: &harness_core::config::server::ServerConfig) -> Self {
        Self {
            absolute_schema_threshold: parse_u64_env(
                "HARNESS_POSTGRES_CATALOG_SCHEMA_THRESHOLD",
                50_000,
            ),
            relative_schema_multiplier: parse_f64_env(
                "HARNESS_POSTGRES_CATALOG_RELATIVE_THRESHOLD_MULTIPLIER",
                3.0,
            ),
        }
    }
}

impl PostgresCatalogMonitor {
    pub(crate) async fn new(pool: PgPool, thresholds: PostgresCatalogThresholds) -> Arc<Self> {
        let initial = match collect_catalog_census(&pool).await {
            Ok(raw) => PostgresCatalogCensus::available(&thresholds, Some(raw.schema_count), raw),
            Err(_) => PostgresCatalogCensus::error(&thresholds, None, "catalog_census_failed"),
        };
        if initial.threshold_breached {
            log_catalog_breach(&initial);
        }
        Arc::new(Self {
            pool: Some(pool),
            thresholds,
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            state: Mutex::new(CatalogMonitorState {
                startup_schema_count: initial.startup_schema_count,
                cached: initial,
                last_refresh_at: Some(Instant::now()),
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn unavailable(
        thresholds: PostgresCatalogThresholds,
        reason: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool: None,
            thresholds,
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            state: Mutex::new(CatalogMonitorState {
                cached: PostgresCatalogCensus::unavailable(&thresholds, reason),
                last_refresh_at: None,
                startup_schema_count: None,
            }),
        })
    }

    pub(crate) async fn snapshot(&self) -> PostgresCatalogCensus {
        let mut state = self.state.lock().await;
        if state
            .last_refresh_at
            .is_some_and(|last| last.elapsed() < self.refresh_interval)
        {
            return state.cached.clone();
        }

        let Some(pool) = self.pool.as_ref() else {
            state.cached =
                PostgresCatalogCensus::unavailable(&self.thresholds, "postgres_pool_unavailable");
            state.last_refresh_at = Some(Instant::now());
            return state.cached.clone();
        };

        let refreshed = match collect_catalog_census(pool).await {
            Ok(raw) => {
                let startup_schema_count = state.startup_schema_count.or(Some(raw.schema_count));
                PostgresCatalogCensus::available(&self.thresholds, startup_schema_count, raw)
            }
            Err(_) => PostgresCatalogCensus::error(
                &self.thresholds,
                state.startup_schema_count,
                "catalog_census_failed",
            ),
        };
        if refreshed.threshold_breached {
            log_catalog_breach(&refreshed);
        }
        state.startup_schema_count = refreshed.startup_schema_count;
        state.cached = refreshed;
        state.last_refresh_at = Some(Instant::now());
        state.cached.clone()
    }
}

impl PostgresCatalogCensus {
    fn available(
        thresholds: &PostgresCatalogThresholds,
        startup_schema_count: Option<u64>,
        raw: RawCatalogCensus,
    ) -> Self {
        let breach_reasons = threshold_breach_reasons(
            raw.schema_count,
            startup_schema_count,
            thresholds.absolute_schema_threshold,
            thresholds.relative_schema_multiplier,
        );
        Self {
            state: PostgresCatalogState::Available,
            schema_count: Some(raw.schema_count),
            catalog_object_count: Some(raw.catalog_object_count),
            database_size_bytes: Some(raw.database_size_bytes),
            sampled_at: Some(raw.sampled_at),
            startup_schema_count,
            absolute_schema_threshold: thresholds.absolute_schema_threshold,
            relative_schema_multiplier: thresholds.relative_schema_multiplier,
            threshold_breached: !breach_reasons.is_empty(),
            breach_reasons,
            error: None,
        }
    }

    fn unavailable(thresholds: &PostgresCatalogThresholds, reason: &'static str) -> Self {
        Self {
            state: PostgresCatalogState::Unavailable,
            schema_count: None,
            catalog_object_count: None,
            database_size_bytes: None,
            sampled_at: None,
            startup_schema_count: None,
            absolute_schema_threshold: thresholds.absolute_schema_threshold,
            relative_schema_multiplier: thresholds.relative_schema_multiplier,
            threshold_breached: false,
            breach_reasons: vec![],
            error: Some(reason.to_string()),
        }
    }

    fn error(
        thresholds: &PostgresCatalogThresholds,
        startup_schema_count: Option<u64>,
        reason: &'static str,
    ) -> Self {
        Self {
            state: PostgresCatalogState::Error,
            schema_count: None,
            catalog_object_count: None,
            database_size_bytes: None,
            sampled_at: Some(Utc::now()),
            startup_schema_count,
            absolute_schema_threshold: thresholds.absolute_schema_threshold,
            relative_schema_multiplier: thresholds.relative_schema_multiplier,
            threshold_breached: false,
            breach_reasons: vec![],
            error: Some(reason.to_string()),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_counts_for_test(
        thresholds: PostgresCatalogThresholds,
        startup_schema_count: Option<u64>,
        schema_count: u64,
        catalog_object_count: u64,
        database_size_bytes: u64,
    ) -> Self {
        Self::available(
            &thresholds,
            startup_schema_count,
            RawCatalogCensus {
                schema_count,
                catalog_object_count,
                database_size_bytes,
                sampled_at: Utc::now(),
            },
        )
    }
}

fn threshold_breach_reasons(
    schema_count: u64,
    startup_schema_count: Option<u64>,
    absolute_schema_threshold: u64,
    relative_schema_multiplier: f64,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if schema_count > absolute_schema_threshold {
        reasons.push("schema_count_gt_absolute_threshold".to_string());
    }
    if let Some(startup) = startup_schema_count.filter(|value| *value > 0) {
        let relative_threshold = (startup as f64) * relative_schema_multiplier;
        if relative_schema_multiplier.is_finite()
            && relative_schema_multiplier > 0.0
            && (schema_count as f64) > relative_threshold
        {
            reasons.push("schema_count_gt_startup_relative_threshold".to_string());
        }
    }
    reasons
}

async fn collect_catalog_census(pool: &PgPool) -> anyhow::Result<RawCatalogCensus> {
    let (schema_count, catalog_object_count, database_size_bytes): (i64, i64, i64) =
        sqlx::query_as(
            "SELECT
                 (SELECT count(*)::BIGINT FROM pg_catalog.pg_namespace) AS schema_count,
                 (SELECT count(*)::BIGINT FROM pg_catalog.pg_class) AS catalog_object_count,
                 pg_database_size(current_database())::BIGINT AS database_size_bytes",
        )
        .fetch_one(pool)
        .await?;
    Ok(RawCatalogCensus {
        schema_count: u64::try_from(schema_count)?,
        catalog_object_count: u64::try_from(catalog_object_count)?,
        database_size_bytes: u64::try_from(database_size_bytes)?,
        sampled_at: Utc::now(),
    })
}

fn log_catalog_breach(census: &PostgresCatalogCensus) {
    tracing::error!(
        schema_count = census.schema_count,
        catalog_object_count = census.catalog_object_count,
        database_size_bytes = census.database_size_bytes,
        startup_schema_count = census.startup_schema_count,
        absolute_schema_threshold = census.absolute_schema_threshold,
        relative_schema_multiplier = census.relative_schema_multiplier,
        breach_reasons = ?census.breach_reasons,
        "postgres catalog census threshold breached"
    );
}

fn parse_u64_env(name: &'static str, default: u64) -> u64 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<u64>() {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(env = name, "invalid Postgres catalog threshold: {error}");
            default
        }
    }
}

fn parse_f64_env(name: &'static str, default: f64) -> f64 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return default;
    }
    match raw.parse::<f64>() {
        Ok(value) if value.is_finite() && value > 0.0 => value,
        Ok(_) => {
            tracing::error!(
                env = name,
                "invalid Postgres catalog threshold: value must be greater than 0"
            );
            default
        }
        Err(error) => {
            tracing::error!(env = name, "invalid Postgres catalog threshold: {error}");
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> PostgresCatalogThresholds {
        PostgresCatalogThresholds {
            absolute_schema_threshold: 50,
            relative_schema_multiplier: 3.0,
        }
    }

    #[test]
    fn absolute_threshold_breach_is_reported() {
        let census =
            PostgresCatalogCensus::from_counts_for_test(thresholds(), Some(20), 51, 100, 1);

        assert!(census.threshold_breached);
        assert_eq!(
            census.breach_reasons,
            ["schema_count_gt_absolute_threshold"]
        );
    }

    #[test]
    fn relative_threshold_breach_is_reported() {
        let census =
            PostgresCatalogCensus::from_counts_for_test(thresholds(), Some(10), 31, 100, 1);

        assert!(census.threshold_breached);
        assert_eq!(
            census.breach_reasons,
            ["schema_count_gt_startup_relative_threshold"]
        );
    }

    #[test]
    fn unavailable_state_has_no_fabricated_counts() {
        let census = PostgresCatalogCensus::unavailable(&thresholds(), "postgres_pool_unavailable");

        assert_eq!(census.state, PostgresCatalogState::Unavailable);
        assert_eq!(census.schema_count, None);
        assert!(!census.threshold_breached);
    }
}
