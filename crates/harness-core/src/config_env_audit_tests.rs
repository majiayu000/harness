use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

struct AllowedRawEnvRead {
    path: &'static str,
    expected_reads: usize,
    reason: &'static str,
}

const ALLOWED_RAW_ENV_READS: &[AllowedRawEnvRead] = &[
    allowed(
        "crates/harness-agents/src/claude_tests.rs",
        1,
        "serialized env fixture",
    ),
    allowed(
        "crates/harness-agents/src/cloud_setup.rs",
        1,
        "agent cloud setup secret passthrough",
    ),
    allowed(
        "crates/harness-agents/src/codex_spawn.rs",
        1,
        "PATH lookup for spawning codex",
    ),
    allowed(
        "crates/harness-agents/src/codex_tests.rs",
        1,
        "serialized env fixture",
    ),
    allowed(
        "crates/harness-agents/src/run_id_tests.rs",
        2,
        "serialized run-id env fixture",
    ),
    allowed(
        "crates/harness-cli/src/commands/exec.rs",
        3,
        "operator identity and sudo process checks",
    ),
    allowed(
        "crates/harness-core/src/agents_md.rs",
        2,
        "operator home instruction discovery",
    ),
    allowed(
        "crates/harness-core/src/db_test_safety.rs",
        1,
        "test database safety override",
    ),
    allowed(
        "crates/harness-core/src/run_id.rs",
        13,
        "run-id process context propagation",
    ),
    allowed(
        "crates/harness-core/src/run_registry.rs",
        2,
        "XDG state path discovery",
    ),
    allowed(
        "crates/harness-observe/src/event_store/store_tests.rs",
        2,
        "serialized run-id env fixture",
    ),
    allowed(
        "crates/harness-rules/src/engine/mod.rs",
        1,
        "operator home rule discovery",
    ),
    allowed(
        "crates/harness-sandbox/src/lib.rs",
        1,
        "PATH lookup for sandbox command execution",
    ),
    allowed(
        "crates/harness-server/build.rs",
        5,
        "Cargo build-script process environment",
    ),
    allowed(
        "crates/harness-server/src/event_replay_tests.rs",
        1,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-server/src/handlers/runtime_hosts_workflow_review_tests.rs",
        1,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-server/src/handlers/token_usage.rs",
        1,
        "operator home usage report discovery",
    ),
    allowed(
        "crates/harness-server/src/hook_enforcer.rs",
        2,
        "CI process detection",
    ),
    allowed(
        "crates/harness-server/src/http/background/runtime_command_dispatch.rs",
        2,
        "operator home fallback for spawned agents",
    ),
    allowed(
        "crates/harness-server/src/http/init.rs",
        3,
        "operator home path discovery",
    ),
    allowed(
        "crates/harness-server/src/http/test_fixtures.rs",
        1,
        "test fixture home directory",
    ),
    allowed(
        "crates/harness-server/src/http/tests/route_helpers.rs",
        1,
        "test git binary override",
    ),
    allowed(
        "crates/harness-server/src/http/tests/runtime_worker_workspace_tests.rs",
        2,
        "test git binary override",
    ),
    allowed(
        "crates/harness-server/src/http/tests/state_support.rs",
        1,
        "test fixture home directory",
    ),
    allowed(
        "crates/harness-server/src/observation_compression.rs",
        1,
        "provider availability check",
    ),
    allowed(
        "crates/harness-server/src/parallel_dispatch_tests.rs",
        1,
        "test git binary override",
    ),
    allowed(
        "crates/harness-server/src/reconciliation_tests.rs",
        1,
        "serialized env fixture",
    ),
    allowed(
        "crates/harness-server/src/router/tests/exec_plan.rs",
        1,
        "test fixture home directory",
    ),
    allowed(
        "crates/harness-server/src/router/tests/observability.rs",
        2,
        "serialized run-id env fixture",
    ),
    allowed(
        "crates/harness-server/src/stdio.rs",
        1,
        "stdio client home fallback",
    ),
    allowed(
        "crates/harness-server/src/task_db/queries_recovery_tests.rs",
        1,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-server/src/task_runner/spawn_tests/mod.rs",
        1,
        "test git binary override",
    ),
    allowed(
        "crates/harness-server/src/task_runner/store/startup.rs",
        2,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-server/src/test_helpers.rs",
        3,
        "test fixture home directory",
    ),
    allowed(
        "crates/harness-server/src/websocket.rs",
        1,
        "websocket client home fallback",
    ),
    allowed(
        "crates/harness-server/src/workspace_test_support.rs",
        1,
        "serialized env fixture",
    ),
    allowed(
        "crates/harness-server/tests/checkpoint_recovery.rs",
        1,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-server/tests/common.rs",
        1,
        "test fixture home directory",
    ),
    allowed(
        "crates/harness-server/tests/interceptor_enforcement.rs",
        1,
        "serialized env fixture",
    ),
    allowed(
        "crates/harness-skills/src/store.rs",
        2,
        "operator home skill discovery",
    ),
    allowed(
        "crates/harness-workflow/src/issue_workflow_store/maintenance.rs",
        1,
        "legacy DATABASE_URL migration gate",
    ),
    allowed(
        "crates/harness-workflow/src/issue_workflow_store_tests.rs",
        2,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-workflow/src/runtime/eval/run.rs",
        2,
        "Postgres integration test gate",
    ),
    allowed(
        "crates/harness-workflow/src/runtime/tests/remote_host_lease.rs",
        1,
        "Postgres integration test gate",
    ),
];

const fn allowed(
    path: &'static str,
    expected_reads: usize,
    reason: &'static str,
) -> AllowedRawEnvRead {
    AllowedRawEnvRead {
        path,
        expected_reads,
        reason,
    }
}

#[test]
fn raw_process_env_reads_stay_allowlisted() {
    let Some(workspace_root) = workspace_root() else {
        return;
    };
    let crates_dir = workspace_root.join("crates");
    let mut actual = BTreeMap::<String, usize>::new();
    collect_raw_env_reads(&crates_dir, &workspace_root, &mut actual);

    let expected = ALLOWED_RAW_ENV_READS
        .iter()
        .map(|entry| (entry.path, entry))
        .collect::<BTreeMap<_, _>>();

    let mut errors = Vec::new();
    for (path, count) in &actual {
        match expected.get(path.as_str()) {
            Some(entry) if *count == entry.expected_reads => {}
            Some(entry) => errors.push(format!(
                "{path}: expected {} raw env reads for {}, found {count}",
                entry.expected_reads, entry.reason
            )),
            None => errors.push(format!(
                "{path}: {count} raw env reads are not allowlisted; route config reads through harness_core::config::process_env or document the process concern here"
            )),
        }
    }

    for entry in ALLOWED_RAW_ENV_READS {
        if !actual.contains_key(entry.path) {
            errors.push(format!(
                "{}: allowlist entry is stale for {}",
                entry.path, entry.reason
            ));
        }
    }

    assert!(
        errors.is_empty(),
        "raw process environment reads changed:\n{}",
        errors.join("\n")
    );
}

fn collect_raw_env_reads(dir: &Path, workspace_root: &Path, actual: &mut BTreeMap<String, usize>) {
    for entry in fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed to read source dir {}: {error}", dir.display()))
    {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "source dir entry under {} should be readable: {error}",
                dir.display()
            )
        });
        let path = entry.path();
        if path.is_dir() {
            collect_raw_env_reads(&path, workspace_root, actual);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }

        let contents = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("failed to read source file {}: {error}", path.display())
        });
        let count = contents.lines().map(raw_env_read_count).sum();
        if count > 0 {
            actual.insert(relative_path(workspace_root, &path), count);
        }
        assert_no_std_env_import_alias(workspace_root, &path, &contents);
    }
}

fn raw_env_read_count(line: &str) -> usize {
    count_substring(line, concat!("std::env::", "var("))
        + count_substring(line, concat!("std::env::", "var_os("))
        + count_bounded_env_call(line, concat!("env::", "var("))
        + count_bounded_env_call(line, concat!("env::", "var_os("))
}

fn count_substring(line: &str, needle: &str) -> usize {
    line.match_indices(needle).count()
}

fn count_bounded_env_call(line: &str, needle: &str) -> usize {
    line.match_indices(needle)
        .filter(|(index, _)| {
            *index == 0
                || !matches!(
                    line.as_bytes()[index - 1],
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b':'
                )
        })
        .count()
}

fn assert_no_std_env_import_alias(workspace_root: &Path, path: &Path, contents: &str) {
    let rel = relative_path(workspace_root, path);
    if rel == "crates/harness-core/src/config/process_env.rs" {
        return;
    }
    assert!(
        !contents.contains(concat!("use std::", "env")),
        "{rel}: import std::env only inside harness_core::config::process_env so the raw env-read audit cannot be bypassed with aliases"
    );
}

fn workspace_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2)?;
    if root.join("Cargo.toml").is_file() && root.join("crates").is_dir() {
        Some(root.to_path_buf())
    } else {
        None
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or_else(|error| {
            panic!(
                "source path {} should be under workspace root {}: {error}",
                path.display(),
                root.display()
            )
        })
        .to_string_lossy()
        .replace('\\', "/")
}
