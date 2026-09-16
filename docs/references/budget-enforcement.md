# Budget Enforcement and Spend Evidence

> Verified: 2026-09-17.
> Linked issue: [GH-1770](https://github.com/majiayu000/harness/issues/1770).
> Scope: existing runtime controls, their execution boundaries, and remaining
> operational acceptance. No production budget is selected here.

## Current behavior

Harness persists workflow usage and implements workflow budget gates, a streamed
usage watchdog, and a daily runtime-profile cap. These controls replace the
historical state in which spend telemetry was observation-only. They do not yet
establish a universal provider-billed USD ceiling across every execution surface.

`RuntimeBudgetPolicy` in `crates/harness-core/src/config/workflow/budget.rs`
defines a workflow ceiling, `shadow`/`enforce`, an explicit `unlimited` opt-out,
an optional daily profile cap, and a daily throttle ratio. Defaults use shadow
mode, `unlimited: false`, and no daily profile cap. Shadow mode records decisions
without stopping work; a built-in numeric default is not an operator-approved
production spend limit.

## Implemented controls

| Boundary | Current implementation | Limit of the evidence |
| --- | --- | --- |
| Durable accounting | `runtime/store/runtime_usage.rs` stores attributed token metrics, integer micro-dollar cost, and `cost_usd_observed` | Provider-reported amounts are not automatically reconciled billing data |
| Before dispatch | `runtime/dispatcher.rs::budget_gate_outcome` checks workflow spend and the configured daily profile cap; shadow records events and enforce defers exhausted work | Comparisons use recorded numerical cost, without rejecting an unobserved aggregate |
| During execution | `workflow_runtime_worker/turn_engine/runtime_usage.rs::budget_stop` checks recorded workflow cost after streamed usage | Unknown or delayed cost cannot prove remaining spend; ordinary stream accounting failures are logged and do not themselves stop execution |
| At completion | `runtime/store/runtime_completion_budget.rs` can replace a decision with a blocked workflow and operator attention under enforce | Ordinary terminal outcomes are preserved; successful agent-contract completions and explicit budget stops have separate handling |
| Backend admission | Agent-contract execution and ordinary `turn_lifecycle.rs` call `enforced_budget_cost_error` on the selected execution backend before launch under enforce | Checks declared cost-reporting capability; later missing or delayed observations still need execution-period handling |
| Agent-contract completion | `agent_contract_stream.rs` rejects an attempt that did not emit observed USD cost under enforce | Cost-reporting capability and an observed event do not establish account attribution or invoice reconciliation |

Workflow and profile comparisons operate on recorded spend. They do not reserve
future spend for concurrently admitted jobs. A cap checked after a usage report
also cannot prevent spend incurred before that report arrived.

## Unknown cost is not zero spend

The usage store aggregates `cost_usd_observed` with `BOOL_AND`, preserving an
unknown component in workflow-level observations. Numeric zero with
`cost_usd_observed: false` remains unknown cost. It is not a free run, and it does
not prove that a dollar ceiling bounded the run.

The inspected backend paths differ:

- Codex oneshot parsing and the Codex per-turn protocol emit unobserved USD cost.
- Claude stream parsing and the direct Anthropic backend emit unobserved USD cost.
- OpenCode has observed-cost event paths in `opencode.rs` and
  `opencode_adapter/protocol.rs`; the latter checks for a USD currency value.
  Selecting that backend alone does not identify the billing account or reconcile
  its reported amounts against billing data.

Ordinary workflow turns now reject a selected cost-blind oneshot or per-turn
backend before launch under an enforced budget policy. Shadow mode and explicit
unlimited policies still permit these backends, while keeping their costs unknown.
The guard checks the backend actually selected for execution, including per-turn
factories; an unused alternative backend cannot satisfy the requirement.

This admission check does not guarantee that a backend declaring cost support
will emit complete, timely observations. The ordinary watchdog and later
dispatch gate still inspect numerical recorded cost without rejecting an
unobserved aggregate. That execution-period coverage remains incomplete.

The ordinary stream path in `turn_engine/helpers.rs` logs usage-persistence or
watchdog errors and continues. A completion comparison against the same ledger
cannot reconstruct a usage event that failed to persist. The agent-contract
stream instead stops on accounting errors. Failure-path verification belongs in
remaining enforcement acceptance.

## Evidence already available

Three new ordinary-admission tests cover eight backend/policy combinations,
including opposite capabilities on the selected and unused execution surfaces.
They and the existing cost-capability guard test passed against an isolated
PostgreSQL database. The assertions verify zero backend calls on rejection and
continued admission under shadow or explicit unlimited policies.

The GH-1770 follow-up records 13 targeted tests against an isolated disposable
PostgreSQL database: cost-blind contract rejection, dispatch gates, completion
ceilings, and daily-profile caps. Those tests used scripted amounts. They prove
those tested boundaries, not actual provider charges, concurrent spend
reservation, or restart behavior. They were not rerun for this documentation
refresh.

The [shared-workflow trial](../shared-workflow-supervised-2026-09-16.md)
retains failed and accepted attempts and reports every attempt's USD cost as
unobserved. The [supervised Docker host trial](supervised-docker-host.md)
retains token usage and intervention evidence while explicitly leaving monetary
cost unknown. Neither trial establishes a paid-cost baseline or a production-wide
spend ceiling.

## Remaining acceptance and disposition

The operator deferred billing-account selection, billing reconciliation, and
production USD-limit decisions until functional work is complete. GH-1770 remains
open for that acceptance. These decisions do not block authorized supervised
local runs, and this document changes no policy.

The remaining sequence is:

1. Verify missing/partial-cost and accounting-error behavior for the execution
   surfaces actually used, with scripted events and a disposable database.
2. Record a real issue/PR workflow's provider-reported or billing-reconciled USD
   cost, model, billing-account attribution, source, and observation status.
   Keep estimates separate from observed amounts.
3. With operator-selected limits, verify dispatch, in-flight interruption,
   completion, restart, failed/retried attempts, and concurrent work. Preserve
   attempts and interventions in the evidence denominator.
4. Document stop/recovery behavior and the remaining reporting delay or
   concurrency limitations before claiming production spend protection.

Do not invent prices, infer subscription charges from token counts, or substitute
unlimited unattended operation for missing cost evidence. Use the existing usage
ledger and policy paths. A new pricing platform, dashboard, or graduated policy
engine is outside this issue's remaining acceptance.
