use super::*;
use harness_core::{
    config::misc::OtelExporter,
    db::pg_open_pool,
    types::{
        AutoFixAttempt, EventMetadata, RuleId, TaskId, TurnFailure, TurnFailureKind, TurnTelemetry,
    },
};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::OnceCell;

// Supabase session pooler caps simultaneous clients at pool_size (15).
// Serialize these DB-backed tests so one test process does not exhaust the
// shared external session budget for the whole workspace.
static DB_SEMAPHORE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
static DB_AVAILABLE: OnceCell<bool> = OnceCell::const_new();

fn db_semaphore() -> Arc<tokio::sync::Semaphore> {
    DB_SEMAPHORE
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
}

async fn db_tests_enabled() -> bool {
    if std::env::var("DATABASE_URL").is_err() {
        return false;
    }

    *DB_AVAILABLE
        .get_or_init(|| async {
            let Ok(database_url) = std::env::var("DATABASE_URL") else {
                return false;
            };
            match tokio::time::timeout(Duration::from_secs(2), pg_open_pool(&database_url)).await {
                Ok(Ok(pool)) => {
                    pool.close().await;
                    true
                }
                _ => false,
            }
        })
        .await
}

fn is_pool_timeout(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("pool timed out while waiting for an open connection")
}

struct TestStore {
    inner: EventStore,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl TestStore {
    async fn close(self) {
        self.inner.close().await;
    }
}

impl std::ops::Deref for TestStore {
    type Target = EventStore;
    fn deref(&self) -> &EventStore {
        &self.inner
    }
}

async fn open_test_store(data_dir: &Path) -> anyhow::Result<Option<TestStore>> {
    if !db_tests_enabled().await {
        return Ok(None);
    }
    let permit = db_semaphore()
        .acquire_owned()
        .await
        .expect("semaphore never closed");
    match EventStore::new(data_dir).await {
        Ok(inner) => Ok(Some(TestStore {
            inner,
            _permit: permit,
        })),
        Err(err) if is_pool_timeout(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn make_event(hook: &str, decision: Decision) -> Event {
    Event::new(SessionId::new(), hook, "Edit", decision)
}

#[tokio::test(flavor = "multi_thread")]
async fn query_empty_store_returns_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let results = store.query(&EventFilters::default()).await?;
    assert!(results.is_empty());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn log_and_query_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let event = make_event("pre_tool_use", Decision::Pass);
    store.log(&event).await?;
    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, event.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn event_metadata_roundtrips() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let mut event = make_event("task_implement", Decision::Complete);
    event.metadata = Some(EventMetadata {
        task_id: Some(TaskId::from_str("task-123")),
        turn: Some(1),
        phase: Some("implement".to_string()),
        telemetry: Some(TurnTelemetry {
            first_token_latency_ms: Some(42),
            completed_latency_ms: Some(84),
            ..Default::default()
        }),
        failure: None,
    });
    store.log(&event).await?;

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    let metadata = results[0]
        .metadata
        .as_ref()
        .expect("metadata should round-trip");
    assert_eq!(
        metadata.task_id.as_ref().map(|id| id.as_str()),
        Some("task-123")
    );
    assert_eq!(
        metadata
            .telemetry
            .as_ref()
            .and_then(|t| t.first_token_latency_ms),
        Some(42)
    );
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_deserializes_rows_without_metadata() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    sqlx::query(
        "INSERT INTO events
            (id, ts, session_id, hook, tool, decision, reason, detail, duration_ms, content, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind("legacy-event")
    .bind(chrono::Utc::now().to_rfc3339())
    .bind(SessionId::new().as_str())
    .bind("legacy_hook")
    .bind("legacy_tool")
    .bind("pass")
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .bind(Option::<i64>::None)
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .execute(&store.pool)
    .await?;

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert!(results[0].metadata.is_none());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn failure_metadata_roundtrips() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let mut event = make_event("agent_review", Decision::Block);
    event.metadata = Some(EventMetadata {
        task_id: Some(TaskId::from_str("task-456")),
        turn: Some(2),
        phase: Some("agent_review".to_string()),
        telemetry: None,
        failure: Some(TurnFailure {
            kind: TurnFailureKind::Upstream,
            provider: Some("anthropic-api".to_string()),
            upstream_status: Some(500),
            message: Some("API returned 500".to_string()),
            body_excerpt: Some("{\"type\":\"error\"}".to_string()),
        }),
    });
    store.log(&event).await?;

    let results = store
        .query(&EventFilters {
            hook: Some("agent_review".to_string()),
            ..Default::default()
        })
        .await?;
    let failure = results[0]
        .metadata
        .as_ref()
        .and_then(|m| m.failure.as_ref())
        .expect("failure metadata should exist");
    assert_eq!(failure.kind, TurnFailureKind::Upstream);
    assert_eq!(failure.upstream_status, Some(500));
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_filters_by_hook() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    store
        .log(&make_event("pre_tool_use", Decision::Pass))
        .await?;
    store
        .log(&make_event("post_tool_use", Decision::Pass))
        .await?;
    let results = store
        .query(&EventFilters {
            hook: Some("pre_tool_use".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].hook, "pre_tool_use");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_filters_by_decision() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    store.log(&make_event("h1", Decision::Pass)).await?;
    store.log(&make_event("h2", Decision::Block)).await?;
    let results = store
        .query(&EventFilters {
            decision: Some(Decision::Block),
            ..Default::default()
        })
        .await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].decision, Decision::Block);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_filters_by_tool() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let sid = SessionId::new();
    store
        .log(&Event::new(sid.clone(), "hook", "tool_a", Decision::Pass))
        .await?;
    store
        .log(&Event::new(sid, "hook", "tool_b", Decision::Pass))
        .await?;
    let results = store
        .query(&EventFilters {
            tool: Some("tool_a".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool, "tool_a");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_filters_by_session_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let sid1 = SessionId::new();
    let sid2 = SessionId::new();
    store
        .log(&Event::new(sid1.clone(), "hook", "tool", Decision::Pass))
        .await?;
    store
        .log(&Event::new(sid2, "hook", "tool", Decision::Pass))
        .await?;
    let results = store
        .query(&EventFilters {
            session_id: Some(sid1.clone()),
            ..Default::default()
        })
        .await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session_id, sid1);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_respects_limit() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    for _ in 0..5 {
        store.log(&make_event("hook", Decision::Pass)).await?;
    }
    let results = store
        .query(&EventFilters {
            limit: Some(3),
            ..Default::default()
        })
        .await?;
    assert_eq!(results.len(), 3);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn persist_rule_scan_logs_one_event_per_violation_under_scan_session() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let violations = vec![
        Violation {
            rule_id: RuleId::from_str("SEC-01"),
            file: std::path::PathBuf::from("src/main.rs"),
            line: Some(42),
            message: "security issue".to_string(),
            severity: Severity::Critical,
        },
        Violation {
            rule_id: RuleId::from_str("U-05"),
            file: std::path::PathBuf::from("src/lib.rs"),
            line: None,
            message: "style issue".to_string(),
            severity: Severity::Low,
        },
    ];
    let session_id = store
        .persist_rule_scan(Path::new("/tmp/project"), &violations)
        .await;
    let events = store.query(&EventFilters::default()).await?;
    assert_eq!(events.len(), 3);
    assert_eq!(events.iter().filter(|e| e.hook == "rule_scan").count(), 1);
    let check_events: Vec<_> = events.iter().filter(|e| e.hook == "rule_check").collect();
    assert_eq!(check_events.len(), 2);
    assert!(check_events
        .iter()
        .all(|event| event.session_id == session_id));
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn persist_rule_scan_logs_summary_even_when_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    store
        .persist_rule_scan(Path::new("/tmp/project"), &[])
        .await;
    let scan_events = store
        .query(&EventFilters {
            hook: Some("rule_scan".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(scan_events.len(), 1);
    assert_eq!(scan_events[0].decision, Decision::Pass);
    let violation_events = store
        .query(&EventFilters {
            hook: Some("rule_check".to_string()),
            ..Default::default()
        })
        .await?;
    assert!(violation_events.is_empty());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn persist_rule_scan_maps_severity_to_decision() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let violations = vec![
        Violation {
            rule_id: RuleId::from_str("R-CRIT"),
            file: std::path::PathBuf::from("a.rs"),
            line: None,
            message: "critical".to_string(),
            severity: Severity::Critical,
        },
        Violation {
            rule_id: RuleId::from_str("R-HIGH"),
            file: std::path::PathBuf::from("b.rs"),
            line: None,
            message: "high".to_string(),
            severity: Severity::High,
        },
        Violation {
            rule_id: RuleId::from_str("R-MED"),
            file: std::path::PathBuf::from("c.rs"),
            line: None,
            message: "medium".to_string(),
            severity: Severity::Medium,
        },
        Violation {
            rule_id: RuleId::from_str("R-LOW"),
            file: std::path::PathBuf::from("d.rs"),
            line: None,
            message: "low".to_string(),
            severity: Severity::Low,
        },
    ];
    store
        .persist_rule_scan(Path::new("/tmp/project"), &violations)
        .await;
    let events = store
        .query(&EventFilters {
            hook: Some("rule_check".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events.len(), 4);
    let by_tool: std::collections::HashMap<_, _> = events
        .iter()
        .map(|e| (e.tool.as_str(), e.decision))
        .collect();
    assert_eq!(by_tool["R-CRIT"], Decision::Block);
    assert_eq!(by_tool["R-HIGH"], Decision::Block);
    assert_eq!(by_tool["R-MED"], Decision::Warn);
    assert_eq!(by_tool["R-LOW"], Decision::Pass);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn persist_rule_scan_stores_project_path_on_anchor() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let project_root = Path::new("/tmp/my-project");
    store.persist_rule_scan(project_root, &[]).await;
    let events = store
        .query(&EventFilters {
            hook: Some("rule_scan".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].detail.as_deref(), Some("/tmp/my-project"));
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn log_auto_fix_report_emits_summary_and_attempt_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let session_id = SessionId::new();

    let report = AutoFixReport {
        attempts: vec![
            AutoFixAttempt {
                rule_id: RuleId::from_str("FIX-01"),
                file: std::path::PathBuf::from("src/main.rs"),
                line: Some(5),
                applied: true,
                resolved: true,
            },
            AutoFixAttempt {
                rule_id: RuleId::from_str("FIX-02"),
                file: std::path::PathBuf::from("src/lib.rs"),
                line: None,
                applied: false,
                resolved: false,
            },
        ],
        fixed_count: 1,
        residual_violations: vec![],
    };

    store
        .log_auto_fix_report(&session_id, &report, Path::new("/tmp/project"))
        .await;

    let all_events = store
        .query(&EventFilters {
            session_id: Some(session_id.clone()),
            ..Default::default()
        })
        .await?;
    assert_eq!(all_events.len(), 3);

    let summary: Vec<_> = all_events.iter().filter(|e| e.hook == "auto_fix").collect();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].decision, Decision::Pass, "no residual = Pass");
    assert_eq!(summary[0].detail.as_deref(), Some("/tmp/project"));

    let attempts: Vec<_> = all_events
        .iter()
        .filter(|e| e.hook == "auto_fix_attempt")
        .collect();
    assert_eq!(attempts.len(), 2);

    let resolved_evt = attempts
        .iter()
        .find(|e| e.tool == "FIX-01")
        .ok_or_else(|| anyhow::anyhow!("FIX-01 attempt event not found"))?;
    assert_eq!(resolved_evt.decision, Decision::Pass);

    let unresolved_evt = attempts
        .iter()
        .find(|e| e.tool == "FIX-02")
        .ok_or_else(|| anyhow::anyhow!("FIX-02 attempt event not found"))?;
    assert_eq!(unresolved_evt.decision, Decision::Block);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn log_auto_fix_report_summary_warns_when_residual_violations_remain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let session_id = SessionId::new();

    let report = AutoFixReport {
        attempts: vec![],
        fixed_count: 0,
        residual_violations: vec![Violation {
            rule_id: RuleId::from_str("R-01"),
            file: std::path::PathBuf::from("src/main.rs"),
            line: None,
            message: "still broken".to_string(),
            severity: Severity::High,
        }],
    };

    store
        .log_auto_fix_report(&session_id, &report, Path::new("/tmp/project"))
        .await;

    let events = store
        .query(&EventFilters {
            hook: Some("auto_fix".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].decision, Decision::Warn);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn log_external_signal_and_query_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let signal = ExternalSignal::new(
        "github".to_string(),
        Severity::High,
        serde_json::json!({"action": "completed", "conclusion": "failure"}),
    );
    store.log_external_signal(&signal)?;
    let results = store.query_external_signals(None)?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, signal.id);
    assert_eq!(results[0].source, "github");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_external_signals_filters_by_since() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let old_signal =
        ExternalSignal::new("github".to_string(), Severity::Low, serde_json::json!({}));
    store.log_external_signal(&old_signal)?;
    let cutoff = chrono::Utc::now();
    let new_signal = ExternalSignal::new(
        "github".to_string(),
        Severity::High,
        serde_json::json!({"conclusion": "failure"}),
    );
    store.log_external_signal(&new_signal)?;
    let results = store.query_external_signals(Some(cutoff))?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, new_signal.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_external_signals_empty_when_no_file() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let results = store.query_external_signals(None)?;
    assert!(results.is_empty());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn log_with_unreachable_otel_endpoint_still_persists_event() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    if !db_tests_enabled().await {
        return Ok(());
    }
    let config = OtelConfig {
        exporter: OtelExporter::OtlpHttp,
        endpoint: Some("http://127.0.0.1:1".to_string()),
        ..OtelConfig::default()
    };
    let permit = db_semaphore()
        .acquire_owned()
        .await
        .expect("semaphore never closed");
    let store = match EventStore::with_policies_and_otel(dir.path(), 1800, 90, &config).await {
        Ok(store) => store,
        Err(err) if is_pool_timeout(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
    assert!(store.otel_pipeline_is_none());
    let event = Event::new(
        SessionId::new(),
        "api_request",
        "http_client",
        Decision::Pass,
    );
    store.log(&event).await?;
    let events = store
        .query(&EventFilters {
            session_id: Some(event.session_id.clone()),
            ..Default::default()
        })
        .await?;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, event.id);
    store.close().await;
    drop(permit);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn migrate_from_jsonl_imports_existing_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let event = make_event("pre_tool_use", Decision::Pass);
    let line = serde_json::to_string(&event)?;
    let jsonl_path = dir.path().join("events.jsonl");
    std::fs::write(&jsonl_path, format!("{line}\n"))?;

    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, event.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn session_renewal_secs_default_is_1800() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    assert_eq!(store.session_renewal_secs(), 1800);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_old_events_zero_days_is_noop() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    store.log(&make_event("hook", Decision::Pass)).await?;
    let deleted = store.purge_old_events(0).await?;
    assert_eq!(deleted, 0);
    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_old_events_removes_stale_and_keeps_recent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let mut old_event = make_event("hook", Decision::Pass);
    old_event.ts = chrono::Utc::now() - chrono::Duration::days(101);
    store.log(&old_event).await?;

    let recent_event = make_event("hook", Decision::Pass);
    store.log(&recent_event).await?;

    let deleted = store.purge_old_events(90).await?;
    assert_eq!(deleted, 1, "only the old event should be purged");

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, recent_event.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_spares_periodic_review_watermarks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let mut old_regular = make_event("pre_tool_use", Decision::Pass);
    old_regular.ts = chrono::Utc::now() - chrono::Duration::days(101);
    store.log(&old_regular).await?;

    let mut old_watermark = make_event("periodic_review:my-project", Decision::Pass);
    old_watermark.ts = chrono::Utc::now() - chrono::Duration::days(101);
    store.log(&old_watermark).await?;

    let deleted = store.purge_old_events(90).await?;
    assert_eq!(deleted, 1, "only the regular old event should be purged");

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, old_watermark.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_trims_old_watermarks_keeps_newest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let hook = "periodic_review:my-project";

    let mut wm_old = make_event(hook, Decision::Pass);
    wm_old.ts = chrono::Utc::now() - chrono::Duration::days(200);
    store.log(&wm_old).await?;

    let mut wm_mid = make_event(hook, Decision::Pass);
    wm_mid.ts = chrono::Utc::now() - chrono::Duration::days(100);
    store.log(&wm_mid).await?;

    let mut wm_new = make_event(hook, Decision::Pass);
    wm_new.ts = chrono::Utc::now() - chrono::Duration::days(1);
    store.log(&wm_new).await?;

    let deleted = store.purge_old_events(90).await?;
    assert_eq!(deleted, 2, "two older watermarks should be trimmed");

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1, "only the newest watermark should remain");
    assert_eq!(results[0].id, wm_new.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn watermark_returns_none_before_first_set() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let result = store.get_scan_watermark("proj", "gc").await?;
    assert!(result.is_none());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_then_get_watermark_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let ts = chrono::Utc::now();
    store.set_scan_watermark("proj", "gc", ts).await?;
    let retrieved = store
        .get_scan_watermark("proj", "gc")
        .await?
        .expect("watermark must exist after set");
    assert_eq!(retrieved.timestamp(), ts.timestamp());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn watermarks_are_per_project_and_agent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let ts = chrono::Utc::now();
    store.set_scan_watermark("proj1", "gc", ts).await?;
    let r1 = store.get_scan_watermark("proj1", "other").await?;
    assert!(r1.is_none(), "different agent_id must not share watermark");
    let r2 = store.get_scan_watermark("proj2", "gc").await?;
    assert!(r2.is_none(), "different project must not share watermark");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_watermark_overwrites_previous() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let ts1 = chrono::Utc::now() - chrono::Duration::hours(1);
    let ts2 = chrono::Utc::now();
    store.set_scan_watermark("proj", "gc", ts1).await?;
    store.set_scan_watermark("proj", "gc", ts2).await?;
    let retrieved = store
        .get_scan_watermark("proj", "gc")
        .await?
        .expect("watermark must exist");
    assert_eq!(retrieved.timestamp(), ts2.timestamp());
    store.close().await;
    Ok(())
}
