# Harness Consolidated Audit

> Merged from `architecture-audit.md` (2026-03-04) and `audit-2026-03-06.md`
> Last updated: 2026-03-06

## Fixed (17 items)

| # | Issue | How Fixed |
|---|-------|-----------|
| A-H1 | Config vs runtime path divergence | `resolve_project_root()` from config |
| A-H5 | GcRun ignores request context | `state.project_root` passed through |
| A-M2 | Skills not reloaded on startup | `discover()` called after `with_persist_dir()` |
| B-1 | ws_token in test_helpers.rs | Field removed, 4 missing fields added |
| B-3 | signal_priority duplicate | Uses `crate::remediation::signal_priority` |
| B-5 | GC task params hardcoded | `gc_adopt_task_request` reads from `GcConfig` |
| B-7 | `PathBuf::from(".")` hardcoded | `state.project_root` + `resolve_project_root()` |
| B-10 | `data_dir()` ignores config | Deleted; uses config path |
| B-12 | `discovery.rs`/`matcher.rs` dead modules | Files deleted |
| B-16 | notify channel silently drops | `record_drop()` with counter + tests |
| B-19 | Unknown status string silent downgrade | `str_to_status` returns error + test |
| B-23 | broadcast channel 256 hardcoded | Reads `notification_broadcast_capacity` from config |
| B-28 | `detect_languages()` never called | Function deleted |
| B-30 | `SessionManager`/`SessionMetrics` unused | Files deleted |
| B-9 | `baseline_violations` synthetic | Re-evaluated: reads real data via `latest_baseline_scan()` |
| B-13 | `unwrap()` on `current_dir()` | Uses `?` with `with_context()` |
| -- | Codex agent CLI flag `-a` → `-s` | Fixed sandbox flag for codex exec |

## Open (15 items)

### Critical (1)

| # | File | Issue |
|---|------|-------|
| 1 | `handlers/mod.rs` | **Dual violation-logging**: `persist_violations()` vs `EventStore::persist_rule_scan()`. Handlers path lacks `rule_scan` anchor event, breaking session-scoped violation counting. Fix: replace all `persist_violations()` calls with `events.persist_rule_scan()`, delete `persist_violations`. |

### High (4)

| # | File | Issue |
|---|------|-------|
| 2 | `thread_manager.rs` | **ThreadManager dead code**: `db: Option<ThreadDb>` field + `open/persist/persist_insert/persist_delete` methods never used. Persistence handled by `AppState.thread_db`. Fix: remove `db` field and all dead persist methods. |
| 3 | `config.rs` | **Unused config fields**: `RulesConfig::discovery_paths`, `RulesConfig::builtin_path`, `ObserveConfig::session_renewal_secs`, `ObserveConfig::log_retention_days` defined but never read at runtime. Fix: wire into startup code or delete. |
| 4 | `anthropic_api.rs:71` | **max_tokens hardcoded** to 4096. Fix: add `max_tokens: u32` to `AnthropicApiConfig`, read from config. |
| 5 | `http.rs:107-109` | **SignalThresholds ignores config**: `SignalThresholds::default()` always used instead of `config.gc.signal_thresholds`. Fix: pass config value to `SignalDetector::new()`. |

### Medium (7)

| # | File | Issue |
|---|------|-------|
| 6 | `anthropic_api.rs:141` | **mpsc send errors silently ignored**: `let _ = tx.send(...)`. Fix: log error on failure. |
| 7 | `observe.rs:46`, `gc.rs:25`, `health.rs:26`, `preflight.rs:59` | **Rule scan errors swallowed**: `scan().unwrap_or_default()` at 4 sites. Fix: log error before fallback. |
| 8 | `prompts.rs:36` | **check_existing_pr hardcodes `/gemini review`**. Fix: make review bot command configurable or add parameter. |
| 9 | `learn.rs:248` | **detect_severity keyword matching** false-positives on "critical path". Fix: check `severity:` field first, fallback to word-boundary regex. |
| 10 | handlers | **Error codes uniformly INTERNAL_ERROR**. Thread not found, draft not found, validation failure all return same code. Fix: add semantic error codes. |
| 11 | `gc_agent.rs:129` | **GC adopt no path validation**: `fs::write(artifact.target_path)` without boundary check. Fix: validate path is within project root. |
| 12 | -- | **No agent selection**: `create_task` always uses `default_agent()`. `dispatch()` exists but never called. Fix: wire dispatch based on task complexity. |

### Low (3)

| # | File | Issue |
|---|------|-------|
| 13 | `task_runner.rs` | **waiting_count TOCTOU**: queries store to count when local counter suffices. Fix: use local `u32`. |
| 14 | 6 files | **make_test_state duplicated**: diverging implementations across test modules. Fix: consolidate to `test_helpers::make_test_state`. |
| 15 | `http.rs:22` | **ExecPlan memory-only**: `HashMap<ExecPlanId, ExecPlan>` lost on restart. Fix: persist to SQLite. |
