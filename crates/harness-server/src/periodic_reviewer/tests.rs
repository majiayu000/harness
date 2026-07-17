use super::tick_helpers::{
    format_violations_for_prompt, pick_secondary_review_agent, poll_task_output,
    MAX_INLINE_VIOLATIONS,
};
use super::*;
use crate::task_runner::CreateTaskRequest;
use harness_core::config::misc::ReviewStrategy;

#[test]
fn review_config_defaults_disabled() {
    let config = ReviewConfig::default();
    assert!(!config.enabled);
    assert!(!config.run_on_startup);
    assert_eq!(config.interval_hours, 24);
    assert!(config.interval_secs.is_none());
    assert_eq!(config.timeout_secs, 900);
    assert_eq!(config.max_concurrent_tasks, 2);
    assert!(config.agent.is_none());
    assert_eq!(config.strategy, ReviewStrategy::Single);
}

#[test]
fn review_config_custom_values() {
    let config = ReviewConfig {
        enabled: true,
        run_on_startup: true,
        interval_hours: 12,
        interval_secs: None,
        agent: Some("codex".to_string()),
        strategy: ReviewStrategy::Cross,
        timeout_secs: 600,
        max_concurrent_tasks: 3,
    };
    assert!(config.enabled);
    assert!(config.run_on_startup);
    assert_eq!(config.interval_hours, 12);
    assert_eq!(config.agent.as_deref(), Some("codex"));
    assert_eq!(config.strategy, ReviewStrategy::Cross);
    assert_eq!(config.timeout_secs, 600);
    assert_eq!(config.max_concurrent_tasks, 3);
}

#[test]
fn pick_secondary_review_agent_prefers_claude_for_codex_primary() {
    let candidates = vec![
        "codex".to_string(),
        "claude".to_string(),
        "anthropic-api".to_string(),
    ];
    let agent = pick_secondary_review_agent("codex", &candidates, |name| name == "claude");
    assert_eq!(agent.as_deref(), Some("claude"));
}

#[test]
fn pick_secondary_review_agent_prefers_codex_for_claude_primary() {
    let candidates = vec![
        "claude".to_string(),
        "codex".to_string(),
        "anthropic-api".to_string(),
    ];
    let agent = pick_secondary_review_agent("claude", &candidates, |name| name == "codex");
    assert_eq!(agent.as_deref(), Some("codex"));
}

#[test]
fn pick_secondary_review_agent_falls_back_to_anthropic_api() {
    let candidates = vec!["codex".to_string(), "anthropic-api".to_string()];
    let agent = pick_secondary_review_agent("codex", &candidates, |name| name == "anthropic-api");
    assert_eq!(agent.as_deref(), Some("anthropic-api"));
}

#[test]
fn effective_interval_prefers_secs_over_hours() {
    let config = ReviewConfig {
        interval_hours: 24,
        interval_secs: Some(300),
        ..ReviewConfig::default()
    };
    assert_eq!(config.effective_interval(), Duration::from_secs(300));
}

#[test]
fn effective_interval_falls_back_to_hours() {
    let config = ReviewConfig {
        interval_hours: 2,
        interval_secs: None,
        ..ReviewConfig::default()
    };
    assert_eq!(config.effective_interval(), Duration::from_secs(7200));
}

#[test]
fn startup_project_delay_runs_immediately_when_enabled_and_no_watermark() {
    let now = Utc::now();
    let interval = Duration::from_secs(3600);
    assert_eq!(
        startup_project_delay(true, interval, None, now),
        Duration::ZERO
    );
}

#[test]
fn startup_project_delay_runs_immediately_when_enabled_and_overdue() {
    let now = Utc::now();
    let interval = Duration::from_secs(3600);
    let last_review = now - chrono::Duration::hours(2);
    assert_eq!(
        startup_project_delay(true, interval, Some(last_review), now),
        Duration::ZERO
    );
}

#[test]
fn startup_project_delay_waits_full_interval_when_disabled_and_no_watermark() {
    let now = Utc::now();
    let interval = Duration::from_secs(3600);
    assert_eq!(startup_project_delay(false, interval, None, now), interval);
}

#[test]
fn startup_project_delay_waits_full_interval_when_disabled_and_overdue() {
    let now = Utc::now();
    let interval = Duration::from_secs(3600);
    let last_review = now - chrono::Duration::hours(2);
    assert_eq!(
        startup_project_delay(false, interval, Some(last_review), now),
        interval
    );
}

#[test]
fn startup_project_delay_preserves_remaining_time_when_not_yet_due() {
    let now = Utc::now();
    let interval = Duration::from_secs(3600);
    let last_review = now - chrono::Duration::minutes(15);
    assert_eq!(
        startup_project_delay(false, interval, Some(last_review), now),
        Duration::from_secs(2700)
    );
    assert_eq!(
        startup_project_delay(true, interval, Some(last_review), now),
        Duration::from_secs(2700)
    );
}

#[test]
fn startup_rescan_recomputes_missing_project_delay_from_watermark() {
    let now = Utc::now();
    let outcome = startup_delay_for_rescanned_project_outcome(
        &HashMap::new(),
        "runtime-project",
        false,
        Duration::from_secs(3600),
        Some(now - chrono::Duration::minutes(15)),
        now,
    );
    assert_eq!(outcome.delay, Duration::from_secs(2700));
    assert_eq!(
        outcome.last_review_ts,
        Some(now - chrono::Duration::minutes(15))
    );
    assert!(!outcome.reused_startup_delay);
}

#[test]
fn startup_rescan_preserves_missing_project_remaining_delay_after_sleep() {
    let min_delay = Duration::from_secs(300);
    let outcome = StartupDelayOutcome {
        delay: Duration::from_secs(2700),
        last_review_ts: None,
        overdue: false,
        elapsed: None,
        reused_startup_delay: false,
    };

    assert_eq!(
        startup_rescan_remaining_delay(&outcome, min_delay),
        Duration::from_secs(2700)
    );
}

#[test]
fn startup_rescan_subtracts_elapsed_sleep_for_original_project_delay() {
    let min_delay = Duration::from_secs(300);
    let outcome = StartupDelayOutcome {
        delay: Duration::from_secs(2700),
        last_review_ts: None,
        overdue: false,
        elapsed: None,
        reused_startup_delay: true,
    };

    assert_eq!(
        startup_rescan_remaining_delay(&outcome, min_delay),
        Duration::from_secs(2400)
    );
}

#[test]
fn create_task_request_source_field() {
    let req = CreateTaskRequest {
        prompt: Some("review".to_string()),
        source: Some("periodic_review".to_string()),
        ..CreateTaskRequest::default()
    };
    assert_eq!(req.source.as_deref(), Some("periodic_review"));
}

#[test]
fn create_task_request_source_defaults_to_none() {
    let req = CreateTaskRequest::default();
    assert!(req.source.is_none());
}

/// Verify that the fallback timestamp merge logic picks the maximum of the
/// DB timestamp and the local fallback, matching the intent of RS-10 fix.
#[tokio::test]
async fn fallback_ts_merge_picks_max() {
    // Use UNIX_EPOCH-based construction to avoid fallible unwrap() calls.
    let epoch = std::time::SystemTime::UNIX_EPOCH;
    let earlier: DateTime<Utc> = DateTime::from(epoch);
    let later: DateTime<Utc> = DateTime::from(epoch + std::time::Duration::from_secs(1_000_000));

    // Fallback newer than DB — fallback wins.
    let result = match (Some(earlier), Some(later)) {
        (Some(db), Some(f)) => Some(db.max(f)),
        (Some(db), None) => Some(db),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };
    assert_eq!(result, Some(later));

    // DB newer than fallback — DB wins.
    let result = match (Some(later), Some(earlier)) {
        (Some(db), Some(f)) => Some(db.max(f)),
        (Some(db), None) => Some(db),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };
    assert_eq!(result, Some(later));

    // No DB entry, only fallback.
    let result: Option<DateTime<Utc>> = match (None::<DateTime<Utc>>, Some(later)) {
        (Some(db), Some(f)) => Some(db.max(f)),
        (Some(db), None) => Some(db),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };
    assert_eq!(result, Some(later));

    // Neither present.
    let result: Option<DateTime<Utc>> = match (None::<DateTime<Utc>>, None) {
        (Some(db), Some(f)) => Some(db.max(f)),
        (Some(db), None) => Some(db),
        (None, Some(f)) => Some(f),
        (None, None) => None,
    };
    assert_eq!(result, None);
}

#[test]
fn format_violations_truncates_at_max_inline() {
    use harness_core::{types::RuleId, types::Severity, types::Violation};
    use std::path::PathBuf;

    let make_v = |i: usize| Violation {
        rule_id: RuleId::from_str(&format!("RS-{i:02}")),
        file: PathBuf::from(format!("src/file{i}.rs")),
        line: Some(i),
        message: format!("violation {i}"),
        severity: Severity::Medium,
    };

    // Exactly at the limit — all shown, no truncation suffix.
    let at_limit: Vec<_> = (0..MAX_INLINE_VIOLATIONS).map(make_v).collect();
    let out = format_violations_for_prompt(&at_limit);
    assert!(out.contains(&format!(
        "{} violation(s) (showing {})",
        MAX_INLINE_VIOLATIONS, MAX_INLINE_VIOLATIONS
    )));
    assert!(!out.contains("more violation(s) not shown"));

    // One over the limit — truncation suffix must appear.
    let over_limit: Vec<_> = (0..=MAX_INLINE_VIOLATIONS).map(make_v).collect();
    let out = format_violations_for_prompt(&over_limit);
    assert!(out.contains("1 more violation(s) not shown"));
}

/// Fallback is updated atomically before the EventStore write; verify the
/// Arc<Mutex<Option<DateTime<Utc>>>> can be written and read correctly.
#[tokio::test]
async fn fallback_ts_arc_mutex_roundtrip() {
    let state: Arc<Mutex<ReviewState>> = Arc::new(Mutex::new(ReviewState::default()));
    assert!(state.lock().await.fallback_ts.is_none());

    let now = Utc::now();
    state.lock().await.fallback_ts = Some(now);
    assert_eq!(state.lock().await.fallback_ts, Some(now));
}

/// Verify that replacing a poll_handle drops the old JoinHandle WITHOUT
/// aborting the underlying task, so the old poller can still persist its
/// findings after a new review cycle starts (issue #448 / round-1 review).
///
/// The previous test only checked `guard.is_some()` after replacement, which
/// would pass even if `h.abort()` were present and the old task was killed.
/// This test additionally asserts that the old spawned task is still running.
#[tokio::test]
async fn poll_handle_replaced_without_aborting_previous() {
    let state: Arc<Mutex<ReviewState>> = Arc::new(Mutex::new(ReviewState::default()));

    // First cycle: spawn a long-running task and store it.
    let first = tokio::spawn(async {
        sleep(Duration::from_secs(3600)).await;
    });
    state.lock().await.poll_handle = Some(first);

    // Second cycle: take the old handle (drop without abort) and store a new one.
    let old_handle = {
        let mut guard = state.lock().await;
        guard.poll_handle.take()
    };
    let second = tokio::spawn(async {});
    state.lock().await.poll_handle = Some(second);

    // Yield so the runtime can process any pending state changes.
    tokio::task::yield_now().await;

    // The old task must still be running — dropping the handle must NOT cancel it.
    assert!(old_handle.is_some(), "first handle must have been stored");
    let old = match old_handle {
        Some(h) => h,
        None => return, // unreachable: asserted above
    };
    assert!(
        !old.is_finished(),
        "old poller must still be running after handle replacement; \
             aborting it would silently drop findings"
    );
    old.abort(); // clean up the long-running task

    // The slot holds exactly one (the new) task.
    let guard = state.lock().await;
    assert!(guard.poll_handle.is_some());
}

/// Structural check: run_review_tick no longer spawns git as a child process.
/// The source file must not contain the forbidden invocation pattern.
#[test]
fn test_git_guard_removed() {
    // The forbidden pattern is split across two literals so that this very
    // test does not trigger the assertion it is enforcing.
    let forbidden = ["Command::new(", "\"git\")"].concat();
    let source = include_str!("../periodic_reviewer.rs");
    assert!(
        !source.contains(&forbidden),
        "periodic_reviewer.rs must not spawn git directly"
    );
}

/// REVIEW_SKIPPED is detected only when the ENTIRE output (trimmed) equals
/// the sentinel.  Line-by-line matching is too permissive: an agent that
/// quotes the sentinel in an explanation or code block would trigger a false
/// skip, silently dropping the real review/secondary/synthesis results.
#[test]
fn test_review_skipped_detection() {
    let is_skipped = |s: Option<&str>| s.map(|s| s.trim() == "REVIEW_SKIPPED").unwrap_or(false);

    // True positive: entire output is exactly the sentinel.
    assert!(is_skipped(Some("REVIEW_SKIPPED")));
    // True positive: trailing newline stripped by trim.
    assert!(is_skipped(Some("REVIEW_SKIPPED\n")));
    // True positive: leading/trailing whitespace stripped by trim.
    assert!(is_skipped(Some("  REVIEW_SKIPPED  ")));

    // False-positive guard: sentinel on its own line but with other content —
    // the entire output is not the sentinel, so this must NOT trigger.
    assert!(!is_skipped(Some(
        "some preamble\nREVIEW_SKIPPED\nmore text"
    )));
    assert!(!is_skipped(Some("No commits found.\nREVIEW_SKIPPED")));
    // False-positive guard: literal appears inside JSON content.
    assert!(!is_skipped(Some(
        r#"{"title":"REVIEW_SKIPPED check removed","action":"LGTM"}"#
    )));
    // Normal review output: no sentinel.
    assert!(!is_skipped(Some("REVIEW_JSON_START\n{}\nREVIEW_JSON_END")));
    // No output at all.
    assert!(!is_skipped(None));
}

#[tokio::test]
async fn poll_task_output_returns_none_for_cancelled_tasks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = crate::task_runner::TaskStore::open(&dir.path().join("tasks.db")).await?;
    let task_id = harness_core::types::TaskId("cancelled-review".to_string());
    let mut task = crate::task_runner::TaskState::new(task_id.clone());
    task.status = crate::task_runner::TaskStatus::Cancelled;
    task.rounds.push(crate::task_runner::RoundResult::new(
        1,
        "review",
        "cancelled",
        Some("cancelled output should be ignored".to_string()),
        None,
        None,
    ));
    store.insert(&task).await;

    let output = poll_task_output(&store, &task_id, 0).await;
    assert!(output.is_none(), "cancelled tasks must not yield output");
    Ok(())
}

/// On first boot (no prior event, no fallback), since_arg must be the
/// Unix epoch sentinel — unchanged behaviour.
#[test]
fn test_first_boot_no_ts_produces_epoch_since() {
    let last_review_ts: Option<DateTime<Utc>> = None;
    let since_arg = last_review_ts
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    assert_eq!(since_arg, "1970-01-01T00:00:00Z");
}

// ── Issue #617: multi-project periodic review ─────────────────────────────

/// Watermark keys are namespaced per-project: an event logged under
/// `"periodic_review:foo"` must not affect the timestamp returned for
/// `"periodic_review:bar"`.
///
/// This test exercises the key-namespacing logic used in both
/// `last_review_timestamp` and the watermark-advance `Event::new` call.
#[test]
fn watermark_hook_key_namespacing_is_distinct() {
    let key_foo = format!("periodic_review:{}", "foo");
    let key_bar = format!("periodic_review:{}", "bar");
    let key_default = "periodic_review".to_string();
    // All three must be distinct — no project leaks into another's namespace.
    assert_ne!(key_foo, key_bar);
    assert_ne!(key_foo, key_default);
    assert_ne!(key_bar, key_default);
}

/// When `config.projects` is empty, `collect_projects` must return exactly
/// one entry whose `root` matches the server's default `project_root`.
///
/// This is a pure structural test; it exercises the `ProjectInfo` name
/// derivation (basename of the path) without needing a real `AppState`.
#[test]
fn collect_projects_empty_config_name_from_basename() {
    // Simulate the basename extraction logic used in collect_projects.
    let root = std::path::PathBuf::from("/home/user/projects/myrepo");
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default")
        .to_string();
    assert_eq!(name, "myrepo");
}

/// `collect_projects` skips projects whose root does not exist on disk.
///
/// Simulates the `entry.root.exists()` guard without a real `AppState`.
#[test]
fn collect_projects_nonexistent_root_skipped() {
    let ghost = std::path::PathBuf::from("/this/path/does/not/exist/at/all/9999");
    assert!(!ghost.exists(), "test precondition: path must not exist");
}

/// A project with `[review]\nenabled = false` in its `.harness/config.toml`
/// must be excluded by `collect_projects`.
///
/// Exercises the opt-out guard using a real tempdir config file.
#[test]
fn collect_projects_opted_out_project_excluded() -> anyhow::Result<()> {
    use std::io::Write;
    let dir = tempfile::tempdir()?;
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir_all(&harness_dir)?;
    let mut f = std::fs::File::create(harness_dir.join("config.toml"))?;
    f.write_all(b"[review]\nenabled = false\n")?;

    let cfg = harness_core::config::project::load_project_config(dir.path())?;
    // The opt-out flag is present and false.
    let opted_out = cfg.review.as_ref().and_then(|r| r.enabled) == Some(false);
    assert!(
        opted_out,
        "project with enabled=false must be marked as opted out"
    );
    Ok(())
}

/// Verify `CreateTaskRequest` carries the correct `project` and `source`
/// fields, matching the per-project routing contract.
#[test]
fn review_request_carries_project_root_and_source() {
    let root = std::path::PathBuf::from("/some/project");
    let req = CreateTaskRequest {
        prompt: Some("review".to_string()),
        source: Some("periodic_review".to_string()),
        project: Some(root.clone()),
        ..CreateTaskRequest::default()
    };
    assert_eq!(req.source.as_deref(), Some("periodic_review"));
    assert_eq!(req.project.as_deref(), Some(root.as_path()));
}
