# Product Spec

## Linked Issue

GH-1467

## User Problem

`harness serve` creates one runtime log file per startup under
`server.data_dir/logs/`. Current main already applies age-based cleanup through
`observe.log_retention_days`, but the default 90-day window still allows many
startup logs to accumulate when operators restart the server often. The issue
was observed with dozens of logs and tens of megabytes still within the age
window.

Operators need runtime logs to stay bounded without losing the most recent log
needed for debugging the current server process.

## Goals

- Preserve existing age-based cleanup controlled by `observe.log_retention_days`.
- Add a count-based runtime log cap so each data-dir `logs/` directory keeps a
  bounded number of `harness-serve-<timestamp>-pid<PID>.log` files.
- Default the cap conservatively to keep the 30 newest runtime logs.
- Allow disabling the count cap explicitly with `0` for operators who want
  age-only retention.
- Keep the active log file and newest matching runtime logs; ignore unrelated
  files such as cloudflared logs or notes.
- Surface cleanup warnings without preventing `harness serve` from starting.
- Document the new config field and local behavior.

## Non-Goals

- Changing runtime log naming or location.
- Compressing, uploading, or archiving old logs.
- Changing event-store retention, workflow runtime retention, task retention, or
  GC behavior.
- Deleting unrelated files in the `logs/` directory.
- Adding a background retention worker; startup cleanup is sufficient for this
  issue.

## User-Visible Behavior

On each `harness serve` startup, Harness should create the current log file,
delete stale runtime logs older than the age window, then delete the oldest
extra runtime logs beyond the configured max-files cap. The current startup log
must remain present. Operators should continue to see a runtime log status block
in health/operator surfaces and warning logs for cleanup entries that could not
be removed.

## Acceptance Criteria

- [ ] `observe.log_retention_max_files` is configurable and defaults to `30`.
- [ ] `observe.log_retention_max_files = 0` disables the count cap while
      preserving existing age-based cleanup.
- [ ] Runtime log cleanup deletes matching stale logs older than
      `log_retention_days`.
- [ ] Runtime log cleanup deletes the oldest extra matching runtime logs beyond
      `log_retention_max_files`.
- [ ] Runtime log cleanup keeps the active/current log and the newest matching
      runtime logs.
- [ ] Runtime log cleanup ignores non-matching files in the same `logs/`
      directory.
- [ ] Delete failures are reported as warnings and do not stop server startup.
- [ ] README and usage-guide docs describe age and max-files retention.
- [ ] Tests cover age pruning, count pruning, disabling the count cap, ignoring
      unrelated files, and deletion warning behavior.

## Edge Cases

- Multiple logs with the same timestamp but different PIDs should sort
  deterministically so count pruning is stable.
- Malformed runtime log filenames should be ignored, not deleted.
- A missing `logs/` directory should produce no warning.
- Retention should run before opening the new log file or otherwise explicitly
  protect the current log from count pruning.

## Rollout Notes

Use `Refs #1467` for the spec PR. The implementation PR should use
`Closes #1467` after the count cap, docs, and tests are merged.
