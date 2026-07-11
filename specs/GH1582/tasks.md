# GH1582 Task Plan

## Linked Issue

GH-1582 (related: GH-1584 auto-recovery escalation consumes these alerts)

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1582-T001` Owner: `core-config` | Done when: `AlertingConfig` (channels, event classes, dedup, queue, heartbeat) lands in `harness-core/src/config/alerting.rs` with `validate()`, env-var secret fallback, `#[serde(default)]` field on `HarnessConfig`, and defaults that keep the feature inert (B-001, B-002) | Verify: `cargo test -p harness-core config`
- [ ] `SP1582-T002` Owner: `core-contract` | Done when: `AlertClass`, `AlertSeverity`, `AlertSubject`, `AlertPayload` (schema_version, dedup_key) exist in `harness-core` with serde snapshot coverage of the stable field set (B-003, B-005, B-020) | Verify: `cargo test -p harness-core alert`
- [ ] `SP1582-T003` Owner: `notify-fix` | Done when: `notify.rs` `record_drop` emits an error-level structured event (rate-limited) in addition to the counter, replacing warn-only logging (B-012) | Verify: `cargo test -p harness-server notify`
- [ ] `SP1582-T004` Owner: `dispatcher` | Done when: `harness-server/src/alerting/` dispatcher exists with bounded non-blocking queue, class filter + dedup cooldown policy, per-channel workers, retry/backoff delivery via mocked transport, per-attempt audit events in the observe store, loop guard (alerting failures never re-raise), and bounded shutdown flush (B-006 to B-011, B-017, B-018) | Verify: `cargo test -p harness-server alerting`
- [ ] `SP1582-T005` Owner: `adapters` | Done when: generic webhook adapter posts the raw JSON payload; Slack and Feishu formatters render the same payload; Feishu tenant-token/send plumbing is extracted from `intake/feishu.rs` into a shared client helper used by both intake and alerting; adapter format errors are failed deliveries; secrets are redacted from audit records (B-013, B-014, B-015) | Verify: `cargo test -p harness-server alerting::adapters feishu`
- [ ] `SP1582-T006` Owner: `producers` | Done when: producers are wired — periodic_retry exhaustion/stuck (`task_failure_exhausted`, once per terminal failure), workflow blocked/failed transitions, circuit-breaker opens, reconciliation anomalies, and ready_to_merge aging honoring `ready_to_merge_alert_ttl_secs` (B-004, B-019, B-020) | Verify: `cargo test -p harness-server periodic_retry reconciliation alerting::producers`
- [ ] `SP1582-T007` Owner: `heartbeat` | Done when: opt-in heartbeat task pings the configured URL on interval via the shared transport; failures log at warn and never self-alert (B-016) | Verify: `cargo test -p harness-server alerting::heartbeat`
- [ ] `SP1582-T008` Owner: `dashboard-visibility` | Done when: `build_alerts` in `handlers/overview.rs` surfaces failed alert deliveries so degraded delivery is visible (B-008) | Verify: `cargo test -p harness-server overview`

## Parallelization

- `SP1582-T001` and `SP1582-T002` first and in parallel (disjoint files in
  harness-core); `SP1582-T003` is independent and can run anytime.
- `SP1582-T004` depends on T001+T002; `SP1582-T005` depends on T004 (adapter
  trait) but the Feishu client extraction can start alongside T004.
- `SP1582-T006`, `SP1582-T007`, `SP1582-T008` are disjoint call sites and
  run in parallel after T004/T005.

## Verification

- `cargo check --workspace`
- `cargo test -p harness-core -p harness-server`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1582`

## Handoff Notes

Feature ships disabled by default; enabling requires an explicit
`[alerting]` section with at least one valid channel. The `notify.rs`
error-level drop fix (T003) and the shared Feishu client helper (part of
T005) are independent hardening and stay even if the alerting module is
pulled. GH-1584 should consume `AlertClass` from harness-core rather than
defining its own escalation event taxonomy. Exact workflow blocked/failed
transition hook sites in the harness-workflow runtime store are marked
"to locate" in tech.md and must be pinned during T006.
