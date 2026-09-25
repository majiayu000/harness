# Status interpretation and workflow recovery — 2026-09-11

The operator explicitly authorized Harness fixes and workflow recovery while
requiring all business Issue/PR changes to remain with Harness-managed Cursor.
No business PR was edited or merged directly by the operator agent in this run.

Harness changes:

- For native local reviews with a declared outcome, and native merge activities,
  descriptive prose no longer overrides structured results. Structured blocker
  checks remain; merge success still requires server-side GitHub verification.
- Local review instructions distinguish pending CI from code defects: a passing
  code review can advance to merge readiness while the server waits for checks.
- Equal or increasing blocker counts no longer stop a repair round. Independent
  review can identify different findings. The existing three-round limit remains.
- Operator recovery removes obsolete current failure/recovery hints, while
  durable prior events and the new recovery event retain the audit history.
- Periodic reconciliation is enabled in production, so externally merged PRs
  are synchronized without manual database updates.

Validation:

- `CARGO_BUILD_JOBS=1 cargo check -p harness-server --all-targets` passed.
- `CARGO_BUILD_JOBS=1 cargo test -p harness-workflow --lib repair_round_policy_counts_rounds_without_requiring_decreasing_findings`
  passed (one direct policy test; no simulated Agent response).
- `CARGO_BUILD_JOBS=1 cargo build -p harness-cli --bin harness` passed.
- Formatting passed. The initial parallel check stalled in compiler processes
  and was interrupted; the sequential retry completed successfully.
- Existing regression expectations were updated for the changed policy but
  simulated Agent-output suites were not run as evaluation evidence. The status
  contract's existing tests moved unchanged apart from the affected prose
  expectation into a sibling file to satisfy the file-size limit.

Production was idle before replacement with `binary/harness-status-recovery`.
Health returned `ok`. Startup reconciliation changed Loom #683, rclean #396,
and argus #212 from stale blocked records to `done`, reflecting actual merges.

Fifteen affected workflows represented fourteen distinct PRs. After deduplicating
subjects and checking GitHub again, eleven still-open PRs were resumed through
`/api/workflows/runtime/unblock`. Three merged PRs were not resubmitted.
Maintainer-frozen tasks and unrelated blockers were not resumed.

Real execution after recovery:

- Autodify #12: independent review identified the remaining merge conflict and
  automatically entered `addressing_feedback`.
- Harness #2051/#2052/#2047: reviews identified current CI or branch-update work
  and automatically dispatched repair to Cursor.
- Loom #680: current-head review passed and reached `ready_to_merge`.
- At observation: ten running activities, one pending, eight direct Cursor
  child processes. These counts do not imply completion of the remaining PRs.

Evidence directory:
`/Users/apple/.local/share/harness/cursor-batches/20260908-233757/status-recovery-20260911/`

Contains the pre-recovery candidates, fresh GitHub checks, recovery responses,
and post-recovery workflow records. Configuration backup contains credentials
and must remain private. Code is deployed but uncommitted. The debug binary's
embedded UI is still a stub because Bun was absent from the build environment;
the separately served dashboard remains the operator UI.
