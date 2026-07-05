use super::*;
use chrono::TimeZone;

#[test]
fn prepare_logging_creates_runtime_log_for_serve() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.data_dir = tempdir.path().to_path_buf();

    let bootstrap = prepare_logging(
        &Command::Serve {
            transport: None,
            port: None,
            project_root: None,
            projects: vec![],
            default_project: None,
        },
        &config,
    );

    assert_eq!(bootstrap.runtime_logs.state.as_str(), "enabled");
    let active_path = bootstrap
        .runtime_logs
        .active_path
        .as_ref()
        .expect("active path");
    assert!(active_path.starts_with(tempdir.path().join("logs")));
    assert!(active_path.exists(), "runtime log file should be created");
    assert_eq!(
        bootstrap.runtime_logs.path_hint.as_deref(),
        Some(active_path.to_string_lossy().as_ref())
    );
    assert_eq!(bootstrap.runtime_logs.retention_max_files, 30);
}

#[test]
fn purge_stale_runtime_logs_keeps_current_and_non_matching_files() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let logs_dir = tempdir.path().join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    let now = Utc
        .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
        .single()
        .expect("timestamp");
    let stale = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("stale timestamp"),
        10,
    );
    let fresh = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 29, 12, 0, 0)
            .single()
            .expect("fresh timestamp"),
        11,
    );
    let unrelated = logs_dir.join("notes.txt");
    fs::write(&stale, "stale").expect("write stale");
    fs::write(&fresh, "fresh").expect("write fresh");
    fs::write(&unrelated, "keep").expect("write unrelated");

    let warnings = purge_stale_runtime_logs(&logs_dir, 30, 30, now, None);

    assert!(warnings.is_empty(), "purge should not warn: {warnings:?}");
    assert!(!stale.exists(), "stale runtime log should be deleted");
    assert!(fresh.exists(), "fresh runtime log should be kept");
    assert!(unrelated.exists(), "non-matching files should be kept");
}

#[test]
fn purge_stale_runtime_logs_prunes_oldest_logs_beyond_count_cap() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let logs_dir = tempdir.path().join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    let now = Utc
        .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
        .single()
        .expect("timestamp");
    let active = runtime_log_path(tempdir.path(), now, 9);
    let same_timestamp_higher_pid = runtime_log_path(tempdir.path(), now, 99);
    let newer = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 29, 12, 0, 0)
            .single()
            .expect("newer timestamp"),
        11,
    );
    let oldest = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0)
            .single()
            .expect("oldest timestamp"),
        12,
    );
    for path in [&active, &same_timestamp_higher_pid, &newer, &oldest] {
        fs::write(path, "runtime log").expect("write runtime log");
    }

    let warnings = purge_stale_runtime_logs(&logs_dir, 30, 2, now, Some(active.as_path()));

    assert!(warnings.is_empty(), "purge should not warn: {warnings:?}");
    assert!(active.exists(), "active runtime log should be protected");
    assert!(
        same_timestamp_higher_pid.exists(),
        "newest matching runtime log should be kept"
    );
    assert!(!newer.exists(), "extra runtime log should be deleted");
    assert!(!oldest.exists(), "oldest runtime log should be deleted");
}

#[test]
fn purge_stale_runtime_logs_keeps_matching_logs_when_count_cap_is_disabled() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let logs_dir = tempdir.path().join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    let now = Utc
        .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
        .single()
        .expect("timestamp");
    let first = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("first timestamp"),
        1,
    );
    let second = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 29, 12, 0, 0)
            .single()
            .expect("second timestamp"),
        2,
    );
    let third = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0)
            .single()
            .expect("third timestamp"),
        3,
    );
    for path in [&first, &second, &third] {
        fs::write(path, "runtime log").expect("write runtime log");
    }

    let warnings = purge_stale_runtime_logs(&logs_dir, 30, 0, now, None);

    assert!(warnings.is_empty(), "purge should not warn: {warnings:?}");
    assert!(first.exists(), "count cap 0 should keep matching logs");
    assert!(second.exists(), "count cap 0 should keep matching logs");
    assert!(third.exists(), "count cap 0 should keep matching logs");
}

#[test]
fn purge_stale_runtime_logs_reports_delete_errors_without_stopping() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let logs_dir = tempdir.path().join("logs");
    fs::create_dir_all(&logs_dir).expect("create logs dir");
    let now = Utc
        .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
        .single()
        .expect("timestamp");
    let stale = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("stale timestamp"),
        10,
    );
    let fresh = runtime_log_path(
        tempdir.path(),
        Utc.with_ymd_and_hms(2026, 4, 29, 12, 0, 0)
            .single()
            .expect("fresh timestamp"),
        11,
    );
    fs::write(&stale, "stale").expect("write stale");
    fs::write(&fresh, "fresh").expect("write fresh");

    let warnings = purge_stale_runtime_logs_with(&logs_dir, 30, 30, now, None, |path| {
        if path == stale.as_path() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated delete failure",
            ));
        }
        fs::remove_file(path)
    });

    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("failed to delete stale runtime log"),
        "warning should identify delete failure: {warnings:?}"
    );
    assert!(stale.exists(), "failed delete should leave stale file");
    assert!(fresh.exists(), "fresh runtime log should be kept");
}

#[test]
fn prepare_logging_disables_file_persistence_for_non_serve_commands() {
    let config = harness_core::config::HarnessConfig::default();
    let bootstrap = prepare_logging(&Command::Version, &config);

    assert_eq!(bootstrap.runtime_logs.state.as_str(), "disabled");
    assert!(bootstrap.runtime_logs.active_path.is_none());
    assert!(bootstrap.runtime_log_file.is_none());
    assert_eq!(bootstrap.runtime_logs.retention_max_files, 30);
}
