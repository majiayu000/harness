# Tech Spec

## Linked Issue

GH-1467

## Product Spec

See `specs/GH1467/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Log path | `crates/harness-cli/src/commands.rs` | `runtime_log_path` writes `harness-serve-<timestamp>-pid<PID>.log` under `server.data_dir/logs/`. | Count pruning must target only this filename family. |
| Startup logging | `prepare_runtime_logs` and `open_runtime_log_file` | Creates `logs/`, calls `purge_stale_runtime_logs`, and opens the active file. | This is the startup cleanup insertion point. |
| Age cleanup | `purge_stale_runtime_logs_with` | Deletes matching runtime logs older than `observe.log_retention_days`; reports deletion errors as warnings. | The new count cap should extend this logic, not replace it. |
| Config | `crates/harness-core/src/config/observe.rs` | `ObserveConfig` has `session_renewal_secs`, `log_retention_days`, and `log_retention_max_files`; default age retention is 90 days and default count retention is 30 files. | Keep serde compatibility for configs without max-files. |
| Metadata | `crates/harness-server/src/server.rs` | `RuntimeLogMetadata` carries `retention_days` for health/operator surfaces. | Consider whether max-files should be exposed in metadata and docs. |
| Docs | `README.md`, `docs/usage-guide.md` | Mention runtime log path and retention window. | Must describe count cap and disable behavior. |
| Tests | `crates/harness-cli/src/commands/runtime_log_tests.rs` | Cover log creation, age pruning, unrelated files, and warning behavior. | Add count-cap coverage. |

## Proposed Design

1. Add `ObserveConfig::log_retention_max_files: usize`.
   - Default: `30`.
   - Serde default should preserve compatibility for existing config files that
     only set `log_retention_days`.
   - `0` disables count pruning.
2. Thread max-files into runtime log metadata and cleanup.
   - Extend `RuntimeLogMetadata` with `retention_max_files`.
   - Update `disabled`, `enabled`, and `degraded` constructors.
   - Update call sites in CLI/server builders and health/operator tests.
3. Extend runtime log cleanup.
   - Parse matching runtime log filenames into `(started_at, pid, path)`.
   - First delete logs older than the age cutoff.
   - Then sort remaining matching logs by `started_at` descending, PID
     descending, and path as a final deterministic tiebreaker.
   - Delete entries beyond `log_retention_max_files` when the cap is nonzero.
   - Keep ignoring non-matching files and directories.
   - Preserve warning collection for read-dir, entry-read, and delete failures.
4. Update docs.
   - Document `observe.log_retention_days`.
   - Document `observe.log_retention_max_files`, default `30`, and `0`
     disables the count cap.
5. Verify with focused CLI tests and workspace gates.

## Data Flow

`harness serve` startup loads config, computes runtime log metadata, creates the
logs directory, prunes old matching runtime log files according to age and count,
opens the current log file, then exposes the runtime log state through existing
health/operator surfaces. No database or HTTP request data flow changes.

## Alternatives Considered

- Age-only retention. Rejected because it is already present and still allows
  high restart counts to accumulate many logs within the age window.
- Byte-size cap. Deferred because count caps are simpler, deterministic, and
  enough to bound per-startup log fanout without scanning file sizes or handling
  partial active writes.
- Background cleanup worker. Rejected because startup cleanup is deterministic
  and the issue is low-severity disk growth.

## Risks

- A too-small default could remove useful logs. Mitigate with a conservative 30
  file default and a `0` disable option.
- Sorting bugs could delete the wrong log. Mitigate with tests for timestamp and
  PID ordering.
- Metadata changes can break health/operator tests. Mitigate with targeted
  `harness-cli` and `harness-server` checks.

## Test Plan

- [ ] Run `cargo test -p harness-cli runtime_log`.
- [ ] Run `cargo test -p harness-core config`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1467`.
- [ ] Run `python3 checks/check_workflow.py --repo .`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` before PR
      merge readiness.

## Rollback Plan

Revert the implementation commit. That removes the max-files config and count
pruning while restoring the previous age-only runtime log cleanup behavior. No
data migration is required.
