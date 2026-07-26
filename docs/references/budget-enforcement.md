# Budget Enforcement and Spend Caps — Analysis

> Date: 2026-07-26
> Linked issue: GH-1770
> Scope: how Harness bounds model spend today, why the current controls
> cannot stop the runaway-cost failure classes already observed in
> operation, and the design direction for server-side budget enforcement.
> Method: read-only code inspection with spot-checked citations.

## Executive Summary

Harness plumbs a USD budget field end to end but almost never sets it,
never enforces it itself, and has no aggregate ceiling of any kind. The
only cost control that reliably binds in production is the turn budget
(`max_turns`, default 20). Spend accounting exists (token-level usage
telemetry, nine usage-monitor handler modules, a detailed dashboard
spec), but it is observation-only: nothing in the dispatch path can
refuse, delay, or throttle work because money is running out. The
operational history — a queue-drain session that accumulated 944M tokens
across 50 compactions, and a 248-session retry storm in 35 minutes when
an upstream OAuth token went invalid — is exactly the class of incident
an admission-level budget gate exists to stop. Both incidents were
bounded by human intervention, not by the system.

## What Exists Today

### 1. The USD budget field: plumbed, defaulted to unlimited

`AgentRequest::max_budget_usd: Option<f64>`
(`crates/harness-core/src/agent.rs:52`) defaults to `None`
(`agent.rs:126`), meaning unlimited. The field survives the full
submission lifecycle: `workflow_runtime_submission/runtime_request.rs`
carries it on both the submission and stored-request forms (lines 53,
159), copies it into the prepared request (line 202), restores it on
redispatch/recovery (line 245), and defaults it to `None` (line 278).
So the plumbing for per-request budgets is done and durable.

Enforcement, however, is delegated entirely to the agent CLI:
`crates/harness-agents/src/claude.rs:148-150` appends
`--max-budget-usd <value>` to the Claude CLI invocation when the field
is set — and only on the batch path. The streaming adapter
(`crates/harness-agents/src/claude_adapter.rs`), which is the runtime's
live spawn path, never passes the flag at all, and neither the Codex
nor the direct Anthropic API adapter receives an equivalent cap.
Harness itself never compares accumulated spend against the budget. So
even for the one field that exists, coverage is one adapter out of
four, and honesty depends on that CLI: the budget is advisory fiction.

### 2. The only subsystem that sets a budget: GC

`crates/harness-core/src/config/misc.rs:159-160` defines
`budget_per_signal_usd` / `total_budget_usd`, defaulted at
`misc.rs:217-218` to $0.50 per signal and $5.00 total. These feed the GC
agent (`harness-gc/src/gc_agent.rs`) and its handler. GC — the
lowest-stakes, lowest-volume consumer in the system — is thus the only
component with a spend ceiling. Issue implementation, PR feedback
repair, review loops, quality gates, planning, and prompt tasks all run
with `max_budget_usd: None`.

### 3. The control that actually binds: turn budget

`crates/harness-core/src/config/concurrency.rs:39-40,90` defines
`max_turns` with a default of 20 (asserted at `concurrency.rs:191`).
The now-deleted legacy task layer (removed from main in PR #1725)
documented in code that the turn budget must be global across the full
task lifecycle rather than reset per retry — a fix for an actual
budget-reset-on-retry bug; that precedent is historical, but the bug
class it names is exactly what B-002 of the companion spec guards
against for USD ledgers. In surviving code, exhaustion surfaces as
explicit outcomes: `turn_budget_exhausted`
(`crates/harness-server/src/workflow_runtime_plan_issue.rs:28,267`) and
`ROUND_BUDGET_EXHAUSTED_REASON`
(`crates/harness-server/src/workflow_runtime_submission/runtime_models.rs:8`).
Candidate fan-out splits the turn
budget across candidates
(`runtime/dispatcher.rs:423 apply_candidate_runtime_budget`, keyed on
`candidate.budget.max_turns_per_candidate`).

Turns are a real but crude proxy for cost: a turn can be a 2k-token
no-op or a 2M-token context-stuffed marathon. The 944M-token session
was not stopped by turn budgeting because queue-drain work item count,
not per-item turns, was the runaway dimension.

### 4. Usage telemetry: rich, observation-only

`crates/harness-observe/src/usage.rs` defines `UsageMetrics`
(`input_tokens`, `output_tokens`, `cache_read_input_tokens`,
`cache_creation_input_tokens`, plus `reported_total_tokens`) and parses
adapter result payloads. The server
exposes nine usage-monitor handler modules
(`handlers/usage_monitor*.rs`: active, aggregate, candidate,
local_usage, process, records, plus `token_usage.rs`), and the web
dashboard has a `UsageMonitor` route. None of this feeds any control
decision.

### 5. The dashboard spec: controls still on paper

`docs/cost-monitoring-dashboard-spec.md` already states the right
principles: attribute usage to workflow-level units rather than process
IDs (spec line 12), "Provide controls to pause, throttle, or cancel
expensive workflow categories before the operator exhausts a quota
window" (line 15), and "Local process sampling is allowed only for
external user-owned … visibility" — the workflow runtime, not process
scanning, must be the source of attribution (lines 39-42). The
pause/throttle/cancel controls, the invocation-keyed usage events
(`agent_invocation_id`), and the confidence separation are all
unimplemented. The current `usage_monitor_process.rs` samples processes
— the very mechanism the spec says must not be the source of truth.

### 6. No token→USD bridge

Usage is counted in tokens; the budget field is denominated in USD;
nothing server-side converts between them. There is no pricing table in
the codebase. The only `Confidence` enum lives in the eval module
(`runtime/eval/model.rs:137`, with `token_confidence` /
`cost_confidence` fields at lines 156-157) — the right concept, in a
module the runtime never consults.

## What Is Missing

| Gap | Consequence |
| --- | --- |
| No default USD budget for issue/PR work | A single misbehaving workflow can spend without bound; only `max_turns` interferes |
| No harness-side enforcement | Budget honesty depends on each agent CLI; Codex and direct-API adapters have no cap at all |
| No global or daily cap | Parallel dispatch multiplies per-workflow spend with no aggregate ceiling; 8 workers × unlimited = unlimited |
| No pre-dispatch budget denial | Retry storms and backlog sweeps are admitted at full rate even while spend is spiking; the 248-session storm was admitted job by job |
| No token→USD conversion | Budgets in USD cannot be evaluated against telemetry in tokens; the field is unenforceable as specified |
| No estimated-vs-confirmed labeling | Any future dollar figure would present estimates as facts, which the cost spec explicitly forbids |

## Operational Grounding

Two incident classes from the 2026-06-30 → 07-04 audit of 132 sessions:

1. **Unbounded queue drain.** Worst session: 72.6MB transcript, 50
   compactions, 944M cumulative tokens, ending mid-queue. A 20-hour
   refactor consumed 8.8M tokens. Fork-per-review multiplied cost ~4×
   per PR. Bounded reviewer lanes (<2M tokens) all behaved well — the
   difference was precisely the presence of a bound.
2. **Retry storm.** 248 sessions spawned in 35 minutes when a proxy
   OAuth token went invalid; every spawn failed fast and was retried
   with no backoff or breaker at admission time. Circuit breakers now
   exist per failure class (`runtime_circuit_breaker.rs`), but they are
   reactive per-profile trippers, not budget-aware admission control;
   a storm of *successful-but-expensive* jobs trips nothing.

Both incidents share a shape: each individual dispatch looked
reasonable; the aggregate was the failure. Only an aggregate,
server-side ceiling addresses that shape.

## Design Direction

### Layered budget model

Three nested ceilings, each with an owner and a default:

1. **Per-activity** — derived share of the workflow budget (analogous
   to how candidate fan-out already splits `max_turns`), bounding a
   single agent invocation.
2. **Per-workflow** — default USD ceiling for every workflow instance,
   overridable per definition/profile in `WORKFLOW.md`. Persisted with
   the instance so redispatch/recovery inherit remaining budget, not a
   fresh grant (the same bug class as budget-reset-on-retry).
3. **Per-profile daily** — global cap per runtime profile per UTC day,
   the aggregate backstop for parallel dispatch.

### Server-side enforcement from streamed usage

The worker already parses adapter usage events (`UsageMetrics`).
Enforcement accumulates cost against the activity/workflow ledgers as
usage arrives, and: (a) continues passing `--max-budget-usd` to CLIs
that support it as a first line of defense, (b) terminates or declines
to extend a turn when the harness-side ledger crosses the ceiling —
uniformly across Claude, Codex, and direct-API adapters.

### Graduated response, not a cliff

Crossing thresholds triggers escalating responses:
soft threshold (e.g. 80%) → throttle (deprioritize new dispatch for the
category); hard per-workflow ceiling → block workflow with
`RequestOperatorAttention` (recoverable via the existing operator
unblock path); daily profile cap → halt new dispatch for that profile
while allowing in-flight turns to finish and `address_pr_feedback`-class
cheap work to proceed. This mirrors the dispatch-barrier reason-code
pattern already in `dispatch_barrier.rs`.

### Pre-dispatch budget gate

The dispatcher consults, before claiming a command: remaining workflow
budget, remaining daily profile budget, and the circuit-breaker state
for the failure class of the target activity. Denials are deferred
dispatches with an explicit reason code (extending the existing
`runtime_policy_disabled | workflow_config_invalid |
isolation_tier_unavailable` set), visible in the runtime tree — not
silent drops.

### Pricing with confidence labeling

A versioned token→USD pricing table (per model, per token class
including cache reads/writes) converts telemetry to dollars. Every
dollar figure carries a confidence label (`Estimated` vs
`ProviderConfirmed`), reusing the eval module's `Confidence` concept at
the runtime layer. Estimates are never presented as provider-confirmed
— the cost spec's own rule, now enforced by type.

## Explicit Non-Goals

- Provider billing reconciliation (invoice-level truth) — out of scope,
  consistent with the cost-monitoring spec's non-goal.
- Per-user quotas or multi-tenant accounting.
- Changing GC's existing `budget_per_signal_usd` / `total_budget_usd`
  mechanics; GC becomes a consumer of the same ledger, not a special
  case, in a later phase.

## Risks and Alternatives

- **Pricing drift.** A stale pricing table under-counts spend. Mitigate
  with a versioned table, a startup staleness warning, and conservative
  (over-estimating) defaults for unknown models.
- **False halts.** A too-low default workflow budget converts long
  legitimate work into operator interrupts. Mitigate by deriving the
  default from observed p95 workflow spend once telemetry exists, and
  shipping enforcement in shadow (log-only) mode first.
- **Alternative considered: turns-only tightening.** Lowering
  `max_turns` and adding per-day turn caps would be simpler but keeps
  the unit mismatch (turns ≠ cost) and cannot express "stop at $N/day",
  which is the operator's actual constraint.

## Related Specs and Files

- `docs/cost-monitoring-dashboard-spec.md` — observability counterpart;
  this analysis covers the enforcement half it deliberately deferred.
- `specs/GH1770/` — product and tech spec for the enforcement work.
- Key code: `crates/harness-core/src/agent.rs`,
  `crates/harness-agents/src/claude.rs`,
  `crates/harness-server/src/workflow_runtime_submission/runtime_request.rs`,
  `crates/harness-workflow/src/runtime/dispatcher.rs`,
  `crates/harness-observe/src/usage.rs`,
  `crates/harness-core/src/config/misc.rs`.
