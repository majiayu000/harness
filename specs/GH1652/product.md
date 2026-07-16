# GH1652 Product Spec: Registry-Driven Definition Enumeration for Operator Surfaces

GitHub issue: `#1652`

## Goals

- Make every operator-facing surface that enumerates workflow definitions
  (operator monitor, monitor sampling, dashboard active counts, overview
  active counts) reflect the full set of definitions registered at
  startup, including declarative definitions from WORKFLOW.md (GH-1609).
- Guarantee a blocked, active, or failed declarative workflow instance is
  as visible to operators as a built-in one.
- Keep built-in-only deployments byte-identical: no output change when no
  declarative definitions are registered.

## Non-Goals

- No workflow runtime, recovery, or registry semantic changes.
- No new dashboard features, panels, or per-definition custom rendering;
  declarative definitions render through the same generic projection as
  built-ins.
- No changes to per-definition behavioral branches (for example
  quality-gate-specific projection logic); only enumeration sites are in
  scope.

## Users

- Operators running unattended deployments who rely on the operator
  monitor and dashboard to find stuck work.
- GH-1609 adopters: anyone running declarative workflow definitions in
  production.

## Behavior Invariants

- `B-001` The set of definition ids used by operator/dashboard
  enumeration comes from the workflow definition registry at request
  time; no operator-facing handler maintains its own definition id list.
- `B-002` With only built-in definitions registered, monitor, sampling,
  dashboard active counts, and overview output are byte-identical to
  today (ordering included).
- `B-003` A nonterminal declarative workflow instance appears in the
  operator monitor payload with the same fields as built-in instances,
  and in dashboard/overview active counts, without declarative-specific
  code in those handlers.
- `B-004` Enumeration order is deterministic (sorted by definition id, or
  the registry's stable order) so payloads and counts do not flap between
  requests.
- `B-005` A definition registered but with zero instances contributes no
  monitor rows and a zero (or absent, matching today's semantics for
  empty definitions) count — never an error.
- `B-006` If the registry is unavailable or empty (cannot happen after a
  healthy startup, which seeds built-ins), enumeration surfaces an
  explicit error path rather than silently rendering an empty dashboard.
- `B-007` The audit closes the class: after this change, no
  operator/dashboard/projection surface in `harness-server` enumerates
  definition ids from a local static list; per-definition behavioral
  matches (not enumerations) are exempt and documented.

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-005 (registered-but-empty definitions), B-006 (empty registry is an explicit error path) |
| Failure paths | covered: B-006; store list errors keep today's warn-and-continue semantics per surface |
| Authorization | N/A — no new endpoints or permissions; same read paths |
| Concurrency | covered: B-004 (deterministic order under concurrent requests); registry reads are the frozen post-startup snapshot |
| Retry + idempotency | N/A — read-only surfaces |
| Illegal transitions | N/A — no state changes |
| Compatibility | covered: B-002 (byte-identical built-in-only output) |
| Degradation / fallback | covered: B-006 (no silent empty dashboard) |
| Evidence / audit | covered: B-007 (enumeration-site audit is an acceptance gate with a documented exemption list) |
| Cancellation / partial | N/A — read-only surfaces |

Cross-product boundary called out explicitly: a declarative instance in
`blocked` (the state most needing attention) must surface in both the
monitor rows and the active counts in the same request cycle (B-003), and
adding a definition must never reorder or rename existing built-in
entries in payload output (B-002 + B-004).
