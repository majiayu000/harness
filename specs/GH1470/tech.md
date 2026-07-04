# Tech Spec

## Linked Issue

GH-1470

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Tick loop | `crates/harness-server/src/periodic_reviewer.rs:1799` | enqueues primary agent every tick per project | Gate goes here |
| Agent-side skip | `periodic_reviewer.rs:878-891,926-939` | agent reports REVIEW_SKIPPED | stays as fallback |
| Secondary gating | `periodic_reviewer.rs` (cross-agent path) | already deferred until commits confirmed | pattern to reuse |
| Review records | review store (`review_store/`) | persists completed reviews | source of last-reviewed anchor |

## Proposed Design

1. Persist the reviewed head SHA (or timestamp) with each completed review
   record if not already present.
2. Before enqueueing the primary agent, resolve the project's current head:
   local workspace `git rev-parse` when a workspace exists, else the head
   recorded by reconciliation/intake if fresh.
3. If current head == last reviewed head → record a local skip (log + metric
   counter) and do not enqueue.
4. If head cannot be determined cheaply → enqueue as today (invariant P3).

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 zero spawn when unchanged | pre-enqueue gate | test: unchanged head → no enqueue |
| P2 observable skip | skip counter | test asserts counter increment |
| P3 conservative fallback | unknown-head branch | test: missing head → enqueue |

## Alternatives Considered

- GitHub API check per tick — rejected: adds REST quota cost and a network
  dependency to a local decision; local head comparison is free.
- Dropping the agent-side REVIEW_SKIPPED — rejected: defense in depth.
