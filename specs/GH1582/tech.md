# GH1582 Tech Spec: External Alerting Channel (Outbound Webhook + Adapters)

Product spec: `specs/GH1582/product.md`
GitHub issue: `#1582`
Related: GH-1584 (auto-recovery escalation consumes these alert classes)

## Codebase Context (verified anchors)

- In-process push: `crates/harness-server/src/notify.rs:18` (`emit`,
  fire-and-forget mpsc `try_send`) and `notify.rs:29` (`record_drop`:
  atomic counter + `tracing::warn!` every 100 drops). This is the
  warn-only drop path the issue requires upgrading to error level plus a
  structured event.
- Feishu plumbing: `crates/harness-server/src/intake/feishu.rs:13`
  (`FeishuIntake` holding a `reqwest::Client`), `feishu.rs:58`
  (`get_tenant_access_token`), `feishu.rs:80` (`send_message`). Config:
  `crates/harness-core/src/config/intake.rs:323` (`FeishuIntakeConfig`,
  `enabled` defaults false, env-var fallback for app_id/app_secret).
- Retry exhaustion / stuck escalation:
  `crates/harness-server/src/periodic_retry.rs:31` (`start`),
  `periodic_retry.rs:42` (`retry_loop`), `periodic_retry.rs:131`
  (`attempt_count < config.max_retries` branch), `periodic_retry.rs:339`
  (enqueue of the `harness:stuck` label agent task). Started from
  `crates/harness-server/src/scheduler.rs` (exact call line to locate at
  implementation time).
- PR feedback sweep:
  `crates/harness-server/src/http/background/pr_feedback.rs:22`
  (`run_runtime_pr_feedback_sweep_tick`), `pr_feedback.rs:94`
  (`ready_to_merge` branch), `pr_feedback.rs:251`
  (`spawn_runtime_pr_feedback_sweeper`).
- Reconciliation aging: `crates/harness-server/src/reconciliation.rs:68`
  (`ready_to_merge_min_age_secs`) and `reconciliation.rs:69`
  (`ready_to_merge_alert_ttl_secs`), consumed at
  `reconciliation.rs:427`.
- Circuit breaker:
  `crates/harness-server/src/runtime_circuit_breaker.rs:81`
  (`CircuitBreakerEventKind`), `runtime_circuit_breaker.rs:88`
  (`CircuitBreakerEvent`), open transitions built around
  `runtime_circuit_breaker.rs:275`.
- Internal dashboard alerts (pull-only, no push):
  `crates/harness-server/src/handlers/overview.rs:655` (`build_alerts`).
- Observe event store: `crates/harness-observe/src/event_store/mod.rs:29`
  (`EventStore`), `event_store/external_signals.rs:11`
  (`log_external_signal`), event insert in `event_store/legacy.rs:44`.
  Core types: `crates/harness-core/src/types.rs:351` (`ExternalSignal`),
  `types.rs:400` (`Event`).
- Config aggregation: `crates/harness-core/src/config.rs:47`
  (`HarnessConfig`; new sections use `#[serde(default)]`). Existing
  sub-config exemplars: `config/observe.rs:4` (`ObserveConfig`),
  `config/misc.rs:636` (`RetrySchedulerConfig`), `config/misc.rs:690`
  (`ReconciliationConfig`).
- AppState: `crates/harness-server/src/http/state.rs:248`.

## Proposed Design

### Config (harness-core, new `config/alerting.rs`)

```toml
[alerting]
enabled = false                    # B-001: inert by default
event_classes = []                 # subset of the closed enum (B-003)
dedup_cooldown_secs = 300          # B-010
queue_capacity = 256               # B-011
shutdown_flush_secs = 5            # B-018

[[alerting.channels]]
name = "ops"                       # audit refers to this name only (B-015)
kind = "webhook"                   # webhook | slack | feishu
url = ""                           # env fallback per channel, intake-style
max_attempts = 3                   # B-006
backoff_base_ms = 500

[alerting.heartbeat]
enabled = false                    # B-016: independently opt-in
url = ""
interval_secs = 60
```

`AlertingConfig` gets a `validate()` called from config load (B-002),
following the `FeishuIntakeConfig` env-fallback pattern for secrets.
New field `pub alerting: AlertingConfig` on `HarnessConfig`
(`#[serde(default)]`, B-001 compat: existing TOML files parse unchanged).

### Contract (harness-core)

```rust
pub enum AlertClass {
    TaskFailureExhausted, WorkflowBlocked, WorkflowFailed,
    CircuitBreakerOpen, ReconciliationAnomaly,
    ReadyToMergeAging, NotifyChannelDrop,
}

pub struct AlertPayload {
    pub schema_version: u32,          // additive-only (B-005)
    pub event_class: AlertClass,
    pub severity: AlertSeverity,      // Error | Warning
    pub timestamp: DateTime<Utc>,
    pub project: Option<String>,
    pub subject: AlertSubject,        // task/workflow/PR ids + entity refs (B-020)
    pub message: String,
    pub dedup_key: String,
}
```

### Dispatcher (harness-server, new module `src/alerting/`)

- `alerting/mod.rs` — `AlertDispatcher`: bounded mpsc queue + one worker
  task per channel (B-017). Producers call a non-blocking
  `dispatcher.raise(payload)`; full queue drops with counter +
  error-level log (B-011), mirroring the `notify.rs` pattern.
- `alerting/policy.rs` — class filter (B-003) + dedup map
  `dedup_key -> last_sent` with cooldown, suppression counter audited
  (B-010).
- `alerting/delivery.rs` — per-channel retry with exponential backoff up
  to `max_attempts`; each attempt logged into the event store via a new
  `alert_delivery` event kind (channel name, attempt, status, duration —
  B-006/B-007/B-008). Delivery/adapter failures are terminal in the
  audit trail and MUST NOT call `raise()` (B-009 loop guard: the
  dispatcher rejects payloads whose class originates from the alerting
  module itself, enforced by construction — delivery code has no path to
  the producer API).
- `alerting/heartbeat.rs` — optional interval ping task (B-016).
- Adapters in `alerting/adapters/`: `webhook.rs` (raw JSON POST),
  `slack.rs` (payload -> Slack blocks text), `feishu.rs` (payload ->
  Feishu text message). The Feishu adapter reuses the tenant-token flow;
  extract `get_tenant_access_token`/`send_message` from
  `intake/feishu.rs:58-102` into a shared `feishu_client` helper used by
  both intake and alerting (single source, no duplicate client).
- Placement rationale: harness-observe is storage/export only; the
  dispatcher needs `AppState`, config, and the Feishu client, so it lives
  in harness-server like `intake/` and `periodic_retry.rs`.

### Producers (wiring)

| Source | Hook point | Class |
|---|---|---|
| Retry exhaustion / stuck | `periodic_retry.rs:131` else-branch and stuck escalation around `:339` | `task_failure_exhausted` |
| Workflow blocked/failed | runtime store state transitions (workflow runtime; exact transition sites to locate in `harness-workflow` runtime store during T006) | `workflow_blocked` / `workflow_failed` |
| Circuit breaker | where `CircuitBreakerEventKind::Opened` events are persisted (`runtime_circuit_breaker.rs:275` consumers) | `circuit_breaker_open` |
| Reconciliation | anomaly transitions in `reconciliation.rs`; aging via existing TTL fields (`reconciliation.rs:68-69`, B-019). The once-per-TTL bound is enforced by the dispatcher's dedup machinery (B-010), not by reconciliation state: `ready_to_merge_open_alert` stays a non-destructive check, and the producer raises with `dedup_key = "<class>:<pr_url>"` and `cooldown = ready_to_merge_alert_ttl_secs`, so repeated sweeps within the window are suppressed without persisting a last-alerted timestamp | `reconciliation_anomaly` / `ready_to_merge_aging` |
| Notify drops | `notify.rs:29` `record_drop` — upgrade to `tracing::error!` + structured event. `notify.rs` stays low-level with no `AppState`/dispatcher dependency: the alerting module owns a small watcher task that periodically reads the existing static drop counter and raises `notify_channel_drop` when the count advanced since the last poll (rate-limited by dedup). No new coupling from `notify.rs` toward alerting | `notify_channel_drop` |

### Data Flow

producer (background loop / state transition)
-> `AlertDispatcher::raise` (non-blocking)
-> policy filter (class allowlist, dedup cooldown)
-> per-channel worker (adapter format -> HTTP POST, retry/backoff)
-> event store audit record per attempt
-> dashboard `build_alerts` (overview.rs:655) extended to read
   `alert_delivery` failures (B-008 visibility).

## Edge Cases

- Alerting about a Feishu outage over the Feishu channel: delivery fails,
  retries, exhausts, audit records failure; no new alert is raised
  (B-009). Other channels still receive the original alert (B-017).
- Alert storm (e.g. circuit breaker flapping): dedup_key =
  `class:entity`, cooldown suppresses repeats with audited counts
  (B-010); bounded queue sheds excess without blocking producers (B-011).
- Server crash with queued alerts: undelivered alerts are lost in-memory
  by design; the heartbeat dead-man switch covers server-down detection
  (B-016). Graceful shutdown flushes with a bounded window (B-018).
- Payload containing secrets: redaction applies the existing
  `redact.rs` conventions before formatting; channel URLs/tokens never
  serialize into audit events (B-015).

## Migration / Compatibility

- `HarnessConfig` gains `alerting` with `#[serde(default)]`; existing
  TOML files and `Default` construction are unaffected (B-001/B-019).
- New event kinds in the observe store are additive; no schema migration
  beyond an idempotent `CREATE TABLE IF NOT EXISTS` / new event kind
  string, consistent with `event_store/migrations.rs` patterns.
- `intake/feishu.rs` refactor keeps `FeishuIntake` public behavior
  identical; only the HTTP client internals move to the shared helper.

## Verification Plan

- Unit: config validation (invalid channel fails, B-002); policy filter
  and dedup with a mocked clock (B-003/B-010); backoff/attempt accounting
  with a mocked HTTP transport — no network in CI (B-006/B-007).
- Loop-guard test: a channel that always fails must terminate with a
  failed audit chain and zero new dispatched alerts (B-009).
- Queue-full test: producers never block; drop counter + error log
  (B-011), mirroring `notify.rs` tests at `notify.rs:87-134`.
- notify upgrade test: drop produces error-level structured event
  (B-012) — extends existing `emit_drops_when_channel_full`.
- Integration: periodic_retry exhaustion fixture raises exactly one
  `task_failure_exhausted` (B-004); ready_to_merge aging respects TTL
  (B-019).
- Shutdown test: pending alerts flushed or marked failed within window
  (B-018).

## Rollback Plan

Feature is config-gated (B-001). Rollback = set `enabled = false` (or
remove the section); no data migration to revert. If the module must be
pulled from a release, delete `src/alerting/` and the `HarnessConfig`
field; the `notify.rs` error-level drop fix and the shared Feishu client
helper stay (independent hardening).

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | `config/alerting.rs` default + server init | `cargo test -p harness-core config::alerting` (default inert) + `cargo test -p harness-server alerting::inert` |
| B-002 | `AlertingConfig::validate()` | `cargo test -p harness-core config::alerting` (invalid channel rejected) |
| B-003 | `alerting/policy.rs` class filter | `cargo test -p harness-server alerting::policy` |
| B-004 | `periodic_retry.rs` producer wiring | `cargo test -p harness-server periodic_retry` (one alert per exhaustion fixture) |
| B-005 | `AlertPayload` serde in harness-core | `cargo test -p harness-core alert_payload` (snapshot of JSON field set) |
| B-006 | `alerting/delivery.rs` backoff | `cargo test -p harness-server alerting::delivery` (mock transport, attempt counts) |
| B-007 | delivery -> event store audit | `cargo test -p harness-server alerting::audit` (per-attempt records) |
| B-008 | audit status + `build_alerts` extension | `cargo test -p harness-server alerting::audit` + `handlers::overview` test for failed-delivery visibility |
| B-009 | dispatcher loop guard | `cargo test -p harness-server alerting::loop_guard` (always-failing channel, zero re-raises) |
| B-010 | `alerting/policy.rs` dedup | `cargo test -p harness-server alerting::dedup` (mocked clock, suppression audited) |
| B-011 | bounded queue in `alerting/mod.rs` | `cargo test -p harness-server alerting::queue_full` (non-blocking, counter + error log) |
| B-012 | `notify.rs` `record_drop` upgrade | `cargo test -p harness-server notify` (error-level structured event on drop) |
| B-013 | adapter error handling | `cargo test -p harness-server alerting::adapters` (format error = failed delivery audit) |
| B-014 | shared `feishu_client` helper | `cargo test -p harness-server feishu` (token failure retried + audited, intake behavior unchanged) |
| B-015 | redaction in payload/audit serialization | `cargo test -p harness-server alerting::redaction` (no URL/token in serialized audit) |
| B-016 | `alerting/heartbeat.rs` | `cargo test -p harness-server alerting::heartbeat` (interval ping via mock transport; failure = warn, no self-alert) |
| B-017 | per-channel workers | `cargo test -p harness-server alerting::multi_channel` (one stalled channel, other delivers) |
| B-018 | shutdown flush | `cargo test -p harness-server alerting::shutdown` (bounded flush, remainder marked failed) |
| B-019 | reconciliation TTL reuse + serde default | `cargo test -p harness-server reconciliation ready_to_merge` + `cargo test -p harness-core config` (old TOML parses) |
| B-020 | payload subject population at producers | `cargo test -p harness-server alerting::producers` (circuit-breaker/reconciliation payloads carry entity refs) |
