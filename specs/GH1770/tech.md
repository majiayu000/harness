# Tech Spec

## Linked Issue

GH-1770

## Current State

### Budget plumbing without enforcement

- `crates/harness-core/src/agent.rs:52,126` — `AgentRequest::max_budget_usd:
  Option<f64>`, default `None` (unlimited).
- `crates/harness-agents/src/claude.rs:148-150` — when set, appended as
  `--max-budget-usd`; enforcement delegated to the Claude CLI. The
  streaming path `crates/harness-agents/src/claude_adapter.rs` never
  passes the flag at all today, and the Codex and direct Anthropic API
  adapters receive no equivalent cap.
- `crates/harness-server/src/workflow_runtime_submission/runtime_request.rs:53,159,202,245,278`
  — the field survives submission, storage, redispatch, and recovery,
  defaulting to `None`.
- `crates/harness-core/src/config/misc.rs:159-160,217-218` — GC is the
  only setter: `budget_per_signal_usd` 0.50, `total_budget_usd` 5.0.

### Real controls today

- Turn budget: `crates/harness-core/src/config/concurrency.rs:39-40,90`
  (`max_turns`, default 20; `concurrency.rs:191` asserts the default),
  global across the task lifecycle. Candidate fan-out splits it:
  `crates/harness-workflow/src/runtime/dispatcher.rs:423`
  `apply_candidate_runtime_budget` reading
  `candidate.budget.max_turns_per_candidate`.
- Circuit breakers per failure class
  (`crates/harness-server/src/runtime_circuit_breaker.rs`) — reactive,
  not budget-aware.
- Dispatch deferral reason codes exist in
  `crates/harness-workflow/src/runtime/dispatch_barrier.rs`
  (`runtime_policy_disabled | workflow_config_invalid |
  isolation_tier_unavailable`).

### Telemetry without control

- `crates/harness-observe/src/usage.rs` — `UsageMetrics` {input_tokens,
  output_tokens, cache_read_input_tokens, cache_creation_input_tokens,
  reported_total_tokens} parsed from adapter result payloads.
- `crates/harness-server/src/handlers/usage_monitor*.rs` (active,
  aggregate, candidate, local_usage, process, records) +
  `token_usage.rs` — read-only monitors. `usage_monitor_process.rs`
  samples OS processes, which
  `docs/cost-monitoring-dashboard-spec.md:39-42` says must not be the
  attribution source of truth.
- No token→USD table anywhere; the only `Confidence` enum is
  `crates/harness-workflow/src/runtime/eval/model.rs:137` (with
  `token_confidence` / `cost_confidence` at 156-157), unused by the
  runtime.

## Design

### 1. Pricing table and cost computation (`harness-core`)

New module `harness-core/src/pricing.rs`:

```rust
pub struct ModelPricing {
    pub model_pattern: String,      // longest-prefix match, e.g. "claude-sonnet-5"
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    pub cache_read_usd_per_mtok: f64,
    pub cache_write_usd_per_mtok: f64,
}

pub struct PricingTable {
    pub version: String,            // e.g. "2026-07"
    pub entries: Vec<ModelPricing>,
    pub fallback: ModelPricing,     // conservative (max of table) for unknown models
}

pub enum CostConfidence { Estimated, ProviderConfirmed }

pub struct CostSample {
    pub usd: f64,
    pub confidence: CostConfidence,
    pub pricing_version: String,
    pub priced_with_fallback: bool,
}
```

Built-in defaults compiled in; overridable by a `pricing` section in the
server config. `cost_for(model, &UsageMetrics) -> CostSample`. If the
adapter payload ever carries a provider-confirmed dollar amount, that
value wins and is labeled `ProviderConfirmed`; otherwise `Estimated`.
Unknown model ⇒ fallback pricing, `priced_with_fallback = true`, and a
rate-limited `warn!`.

### 2. Budget configuration (`WORKFLOW.md` / config)

Extend the runtime config front-matter:

```yaml
runtime_budget_policy:
  default_workflow_budget_usd: 15.0     # built-in default if section absent
  soft_threshold_ratio: 0.8
  daily_profile_cap_usd: 200.0
  daily_throttle_ratio: 0.8
  enforcement: shadow | enforce         # default shadow
  unlimited: false                      # explicit opt-out only
```

Precedence for a workflow instance's budget: submission override
(`runtime_request.max_budget_usd`) > definition/profile
`runtime_budget_policy` > built-in default. `None` at every layer no
longer means unlimited; only `unlimited: true` does.

### 3. Ledgers (Postgres, `harness-workflow` store)

Additive tables in the runtime store migrations
(`runtime/store_migrations.rs`):

- `workflow_budget_ledger(workflow_id PK, budget_usd, spent_usd,
  pricing_version, threshold_state, updated_at)` — one row per
  instance; `spent_usd` monotonically increased in the same transaction
  that commits activity completion
  (`commit_runtime_activity_completion_*`), so retry/recovery paths
  inherit it for free (B-002).
- `profile_daily_spend(profile, utc_day, spent_usd, state, PRIMARY KEY
  (profile, utc_day))` — upserted with `spent_usd = spent_usd + $n`;
  concurrent workers tolerate small overshoot from in-flight turns.

Per-activity share: `activity_budget_usd = remaining_workflow_budget /
expected_remaining_activities` with a floor; candidate fan-out divides
the activity share across candidates, mirroring the existing
`max_turns_per_candidate` split in `dispatcher.rs:423`.

### 4. Enforcement points

1. **Pre-dispatch gate** (`runtime/dispatcher.rs`): before dispatching
   a command, load ledger + daily row. Exhausted workflow budget ⇒
   defer with new barrier reason `workflow_budget_exhausted`; daily cap
   ⇒ `profile_daily_cap_reached`; throttle band ⇒ deprioritize
   (dispatch only when no under-threshold work is claimable). Reasons
   extend the existing `dispatch_barrier.rs` enum and surface through
   the same status paths.
2. **Adapter flag** (`claude.rs` AND `claude_adapter.rs`): populate
   `max_budget_usd` from the activity share when unset. Today only the
   batch path (`claude.rs:148-150`) forwards the flag; the streaming
   adapter must gain the same argument, and per the project CLI-arg
   rule any arg-construction change applies to both files, verified by
   `cargo test --package harness-agents`. Codex/direct-API adapters
   skip the flag; server-side enforcement covers them.
3. **Turn-stream watchdog** (`workflow_runtime_worker`): accumulate
   `CostSample`s from streamed usage events; when the activity share is
   crossed mid-turn, request turn interruption via the adapter's
   existing interrupt path (Codex `interrupt`; process termination for
   batch CLIs) and record outcome
   `ActivityErrorKind::BudgetExhausted`-equivalent (see §5). This is a
   deliberate departure from
   `docs/cost-monitoring-dashboard-spec.md`'s "gate new dispatch only"
   posture: hard per-activity ceilings terminate in-flight turns,
   because an activity that has already blown its share is precisely
   the case dispatch-time gating cannot catch.
4. **Hard workflow ceiling** (reducer): when a completed activity's
   ledger commit crosses `budget_usd`, the reducer emits
   `RequestOperatorAttention` with reason `budget_exhausted` instead of
   scheduling further activities; existing operator unblock + config
   raise recovers (reuses `OperatorGate` machinery; no new lifecycle
   states).
5. **Shadow mode**: all four points compute and log
   (`budget_shadow_decision` runtime events) but take no action unless
   `enforcement: enforce` for the profile.

### 5. Typed outcomes

- New dispatch-barrier reasons: `workflow_budget_exhausted`,
  `profile_daily_cap_reached`, `profile_daily_throttled`.
- New runtime event kinds: `budget_threshold_crossed`,
  `budget_shadow_decision`, `budget_stop`.
- Activity stop reason distinct from `turn_budget_exhausted`; classified
  non-retryable-without-operator (maps to the existing
  blocked/operator-attention path, not the transient retry path in
  `reducer/runtime_failure.rs`).

### 6. Observability

- Workflow status payloads gain `budget_usd`, `spent_usd`,
  `threshold_state`, `confidence`.
- `handlers/usage_monitor_aggregate.rs` gains per-profile daily spend +
  cap + ladder state.
- All dollar fields serialize alongside their `confidence` and
  `pricing_version` (B-007).

## Migration / Rollout Order

1. Land pricing table + ledger tables + ledger writes (shadow only;
   additive migration).
2. Wire shadow decisions at all four enforcement points; observe for a
   week; tune `default_workflow_budget_usd` from observed p95 workflow
   spend.
3. Enable `enforce` for one low-volume profile; then broadly.
4. Enable daily caps last.

Rollback at any phase = revert config to `shadow`; tables and events
remain (harmless).

## Validation

- `cargo test -p harness-core pricing` — table lookup, fallback,
  confidence.
- `cargo test -p harness-workflow` — ledger commit atomicity with
  activity completion; dispatcher gate defers with typed reasons;
  reducer operator-attention on ceiling; no ledger reset across
  retry/redispatch/recovery (regression guard for the
  budget-reset-on-retry bug class documented in the legacy task layer
  before its removal in PR #1725).
- `cargo test -p harness-server` — shadow vs enforce behavior, monitor
  payload fields, daily rollover.
- `cargo test -p harness-agents` — flag derivation (both adapter files
  per the CLI arg-order rule), absence for non-supporting adapters.
- Full gate before PR: `cargo clippy --workspace --all-targets -- -D
  warnings` + `cargo fmt --all`.

## Out of Scope (tracked elsewhere)

- Provider billing reconciliation (explicit non-goal of
  `docs/cost-monitoring-dashboard-spec.md` as well).
- Invocation-keyed usage event contract (`agent_invocation_id`) from the
  cost-monitoring spec — complementary; this spec only adds ledger
  fields to existing surfaces.
- GC budget unification — GC keeps `budget_per_signal_usd` /
  `total_budget_usd` untouched.
