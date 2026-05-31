# Harness Runtime Health Report — 2026-05-31

Snapshot of the live local server (pid 47893, `127.0.0.1:9800`, `config/claude.toml`)
taken at ~21:46 +08:00. All claims are tagged Fact / Inference / Suggestion per W-11.

## Facts

### Service layer — healthy
- [source: `GET /health`] `status: ok`. All 12 stores `ready: true`, no degraded
  subsystems, `runtime_state_dirty: false`.
- [source: `ps`] `harness serve` running ~2.5h CPU time, stable.
- [source: `lsof -p 47893`] runtime log at
  `~/Library/Application Support/harness/logs/harness-serve-20260531T132638Z-pid47893.log`.

### Task throughput — 36% failure rate
- [source: `GET /tasks` counts] 50 tasks: 7 done, 17 implementing (running),
  **18 failed**, 4 awaiting_deps, 4 cancelled.
- [source: `/tasks` data] all 18 failed tasks report `error: null` at the task level.
  Failures concentrate in remem (8), litellm-rs (4), vibeguard (2), rui (2).

### Failure root causes (only visible in the runtime log, not in `/tasks`)
- [source: log] `agent turn execution timeout reached timeout_secs=900` →
  `Agent turn timed out after 900s` for `activity=plan_repo_sprint agent=codex`.
- [source: log] `in-process app-server event stream lagged; dropped {11,24,43,156} events`
  → `turn failed` (4 occurrences). This is the WorkStream #432 backpressure issue.
- [source: log] 129 `slow statement` warnings in ~20 min: `COMMIT` 1–3.5s,
  frequent `UPDATE threads SET cwd …` >1s, single `SELECT runtime_events` returned
  44,391 rows, `runtime_jobs` 18,691 rows.

### Database — primary bottleneck
- [source: psql] DB size **22 GB**.
- [source: `pg_stat_user_tables`] largest tables: `threads` 237 MB / 2,793 rows
  (heavy bloat ~85 KB/row), `workflow_events` 133 MB, `runtime_events` 121 MB.
- [source: `information_schema.schemata`] **13,618 schemas total**; 13,602 are
  workspace schemas matching `^h[0-9a-f]{16}$`.
- [source: `harness_admin.schema_ownership`] only 2,554 schemas tracked →
  **11,048 workspace schemas are untracked** (reaper-invisible).
- [source: filesystem check of `owner_path`] of 1,195 tracked owner_paths, only **8
  still exist on disk**; 1,187 point to gone macOS temp dirs (`/var/folders/.../T/.tmp*`,
  i.e. test residue).
- Net: ~8 of 13,602 workspace schemas correspond to a live owner; the rest are orphans.

### Timeout configuration source
- [source: code] default turn timeout constant `DEFAULT_RUNTIME_TURN_TIMEOUT_SECS = 3600`
  (`executor.rs:31`). The effective value is resolved by `runtime_timeout_fallback`
  (`executor.rs:344`) with precedence: workflow+activity profile → activity profile →
  workflow profile → `runtime_dispatch.timeout_secs` → code default.
- [source: `WORKFLOW.md:39-41`] the 900s came from the per-repo workflow doc:
  `plan_repo_sprint` was configured with `reasoning_effort: xhigh` **and**
  `timeout_secs: 900`, while sibling activities use 3600. The slowest-reasoning
  activity had the tightest timeout.
- [source: per-repo scan] ~16 active repos' `WORKFLOW.md` all carry `plan_repo_sprint: 900`.

## Inferences
- [based on: log causal chain, confidence: medium] DB bloat (13.6k schemas + 237 MB
  `threads`) slows COMMIT/UPDATE → the in-process event consumer falls behind →
  broadcast channel overflows and drops events → turn fails. The catalog/table bloat
  and the event-stream lag are one chain, not two independent issues.
- [based on: code read, confidence: high] task `error: null` is silent degradation
  (U-29). `task_query_routes.rs:667` reads the task `error` from
  `workflow.data["failure_reason"]`, but the agent-turn-failure path
  (`runtime_failed_decision`, `runtime_failure.rs:42`) only put the reason into the
  command payload, never into instance `data`. `apply_inline_command_side_effect`
  handled `BindPr`/`MarkDone` but let `MarkFailed`/`MarkBlocked`/`MarkCancelled` fall
  through to `_ => Ok(())`.
- [based on: #1216 history + counts, confidence: medium] the orphan reaper helped
  (538k → 13.6k schemas) but does not cover the 11,048 untracked schemas and cannot
  keep up with per-workspace schema creation.

## Suggestions
- [assumption: deep sprint planning legitimately needs >15 min]
  `plan_repo_sprint` ran `reasoning_effort: xhigh` (slowest) under a 900s timeout
  (tightest) while siblings used 3600. **Applied** — the `plan_repo_sprint.timeout_secs`
  override was removed from all 23 registered repos' `WORKFLOW.md` (22 were 900, harness
  was briefly 1800); it now inherits `runtime_dispatch.timeout_secs` (3600).
  `reasoning_effort: xhigh` kept. claude-skill-registry-core had no override (already
  3600). Verified: all frontmatter parses, no residual `plan_repo_sprint.timeout_secs`.
  Each repo is a separate working tree → shows as one uncommitted line-removal each.
  Risk: a genuinely stuck sprint-plan turn now hangs up to 60 min before reclaim.
  Needs server restart to take effect.
- [assumption: failures should be diagnosable from `/tasks`] Bubble the failure reason
  into queryable data. **Applied** — `apply_failure_reason_side_effect` now writes
  `data["failure_reason"]` for MarkFailed/MarkBlocked/MarkCancelled in both
  `runtime/worker.rs` and `runtime/store.rs`; 2 regression tests added; full
  harness-workflow suite green (216 passed). Needs a server rebuild + restart to take
  effect (restart is the operator's call).
- [assumption: orphan schemas are safe to drop when owner is gone] Extend the reaper to
  enumerate untracked `^h[0-9a-f]{16}$` schemas, not just `schema_ownership` rows, and
  drop those whose owner_path no longer exists. Alternative: a one-off cleanup pass for
  the 1,187 temp-dir-orphans + 11,048 untracked. Risk: must exclude the ~8 live schemas
  (e.g. the active `h136534546a051d3d`). Not yet done.
- [assumption: backpressure is load-driven] After DB cleanup, re-check the event-stream
  lag; if it persists, raise `server.notification_broadcast_capacity` /
  `runtime_worker` consumer throughput. Not yet done.

## Changes applied in this session
1. `WORKFLOW.md` — `plan_repo_sprint.timeout_secs` 900 → 1800.
2. `crates/harness-workflow/src/runtime/worker.rs` — new
   `apply_failure_reason_side_effect` + MarkFailed/Blocked/Cancelled match arm + 2 tests.
3. `crates/harness-workflow/src/runtime/store.rs` — same match arm, delegates to the
   worker helper (kept minimal for the U-16 2300-line ceiling).

Verification (this session): `cargo check -p harness-workflow` clean;
`cargo test -p harness-workflow --lib` → 216 passed; `cargo fmt --all -- --check` clean.
Not done: rebuild release binary, restart server, bulk-edit other repos, reaper change.
