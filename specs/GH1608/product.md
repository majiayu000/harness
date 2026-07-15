# GH1608 Product Spec: Runtime-Registered Workflow Definition Registry

GitHub issue: `#1608`

## Goals

- Replace the compile-time static workflow state tables with a registry of
  owned definitions populated at startup, so a later feature (GH-1609) can
  register configuration-defined workflow definitions without Rust edits.
- Register each definition's state table and its transition allowlist
  together, giving one lookup source for "what states exist" and "what
  transitions are legal".
- Preserve observable behavior exactly: the four built-in definitions,
  their states, terminal mappings, and transition rules do not change.

## Non-Goals

- No new definition sources: no WORKFLOW.md parsing, no config schema
  (GH-1609 owns that).
- No reducer generalization; hardcoded completion logic stays as-is.
- No persisted-schema, wire-format, or dispatch changes.

## Users

- GH-1609 (declarative definitions), which needs a registration API.
- Harness developers adding future built-in definitions with less
  boilerplate and a single registration point.

## Behavior Invariants

- `B-001` Behavior-preserving: every existing `harness-workflow` and
  `harness-server` test passes unchanged; an equivalence test asserts the
  registered built-in state sets, terminal mappings, and transition
  allowlists are identical to the previous static tables.
- `B-002` Registry entry types own their strings; no `&'static str`
  remains in `WorkflowStateKey` / `WorkflowStateDefinition` or the
  registration API.
- `B-003` The four built-in definitions are seeded exactly once during
  registry initialization, before any dispatcher, worker, or store
  consumer can perform a lookup.
- `B-004` Registering a definition id that already exists fails loudly
  (error, not warning); there is no silent replacement.
- `B-005` Lookups for unknown definition ids behave exactly as today:
  empty state slice, `None` terminal state, no panic.
- `B-006` Registry reads take no lock across await points and add no
  measurable overhead to the dispatch/completion hot path (read-only
  after initialization or read-locked with short critical sections).
- `B-007` The registry freezes after startup registration completes;
  attempts to register once workflow processing has started are rejected
  with an error, so the set of definitions is stable for the lifetime of
  the process.
- `B-008` Each registered definition carries its transition allowlist;
  validator selection resolves through the registry rather than a
  hardcoded match, with rule content unchanged for built-ins.

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-005 (unknown id lookups keep today's empty/None semantics) |
| Failure paths | covered: B-004 (duplicate registration errors), B-007 (late registration rejected) |
| Authorization | N/A — in-process API only; no new external surface |
| Concurrency | covered: B-006 (no lock across await, short read sections), B-003/B-007 (registration ordered before consumers) |
| Retry + idempotency | covered: B-004 (re-registration is an explicit error, not an implicit upsert) |
| Illegal transitions | covered: B-008 (allowlists preserved verbatim for built-ins) |
| Compatibility | covered: B-001 (full-suite equivalence gate), B-002 (API shape change is compile-time only) |
| Degradation / fallback | covered: B-004/B-007 (loud failure, never silent partial registry) |
| Evidence / audit | covered: B-001 (equivalence test is the recorded proof of preservation) |
| Cancellation / partial | covered: B-007 (freeze point makes partially-registered states impossible to observe) |

Cross-product boundary called out explicitly: initialization order — the
registry must be complete and frozen before the first store lookup, and a
failed built-in registration must abort startup rather than start a
server with a partial registry (B-003 + B-004 + B-007).
