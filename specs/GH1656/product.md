# GH1656 Product Spec: Source-Agnostic Intake Bindings for Declarative Definitions

GitHub issue: `#1656`
Depends on: GH-1609 (declarative definitions, merged). Related: GH-1652
(declarative observability), GH-1607 (agent-as-probe continuation).

## Goals

- Let a WORKFLOW.md `definition` declare where its instances come from: an
  optional `intake` binding routes incoming issues from any registered
  intake source into the declarative definition, completing the
  "give a WORKFLOW.md and work arrives by itself" loop.
- Keep the binding schema strictly source-agnostic: filters operate on the
  normalized `IncomingIssue`, never on GitHub-specific structures.
  Trackers such as Linear are optional future sources behind the existing
  `IntakeSource` trait and must need zero binding-schema changes.
- Preserve today's intake behavior exactly for unmatched issues and for
  deployments without bindings.
- Bound unattended instance creation: per-binding dedupe and an
  active-instance cap, with every skip audited.

## Non-Goals

- No new tracker sources (Linear or otherwise) in this feature.
- No external-state lifecycle mirroring (auto-cancel on issue close);
  workflows observe external state through their own activities.
- No changes to github_issue_pr intake for unmatched issues.
- No per-binding scheduling or priority beyond the active-instance cap.

## Users

- Repo owners running custom declarative flows who today must feed
  instances through the submission API by hand.
- Operators running unattended multi-repo deployments who need custom
  flows to be discovered, deduped, and capped like built-in ones.

## Behavior Invariants

- `B-001` A definition without an `intake` block, and any deployment with
  no bindings at all, produces byte-identical intake behavior to today —
  routing, coverage gating, and dispatch of github_issue_pr included.
- `B-002` Binding validation is fail-closed at startup: unknown or
  disabled `source`, empty `filter.labels`, or `max_active_instances < 1`
  aborts startup with an actionable error and zero partial registration.
- `B-003` The binding schema is source-agnostic: matching consumes only
  normalized `IncomingIssue` fields (`labels`, `source`, `repo`); nothing
  in the schema or matcher references GitHub-specific data. A
  non-GitHub-source routing path is exercised by test.
- `B-004` Routing is deterministic: bindings match by declared source and
  label filter; an issue matching multiple bindings routes to the
  lexicographically smallest definition id and the tie is recorded in the
  submission audit (same rule as GH-1609 signal precedence).
- `B-005` A matched issue creates the instance through the existing
  declarative submission path with the pinned definition version; the
  instance subject carries external id, repo, url, and title, and
  `author_trust_class` flows through unchanged.
- `B-006` Unmatched issues follow the existing github_issue_pr path with
  no behavior change; a matched issue never *also* dispatches to
  github_issue_pr (routing is exclusive).
- `B-007` Dedupe: at most one nonterminal instance per
  `(definition_id, external_id)`. A live instance suppresses re-dispatch
  silently-visibly (audited as suppressed, not dropped); a terminal
  instance permits a new one.
- `B-008` `max_active_instances` is enforced per binding: at cap, new
  matches are skipped with an audited reason and naturally revisited on
  later poll ticks — never enqueued into an unbounded backlog and never
  silently dropped.
- `B-009` Routing decisions, suppressions, and cap-skips are observable:
  each produces an audit record naming the issue, the binding, and the
  outcome; declarative instances born from intake are visible on operator
  surfaces per GH-1652.
- `B-010` The harness process gains no tracker/API calls from this
  feature: bindings only consume what registered sources already poll.
- `B-011` Intake-born instances are ordinary declarative instances:
  pinning, recovery, cancel, and continuation semantics apply without
  special cases; nothing distinguishes them at runtime except subject
  metadata.
- `B-012` A source that is registered but yields an issue whose repo maps
  to a project without the bound definition does not create an instance
  and does not error the tick: the mismatch is audited and the issue
  follows the unmatched path (definitions are per-project; bindings match
  within their own project scope only).

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-001 (no bindings = today), B-002 (empty label filter rejected) |
| Failure paths | covered: B-002 (fail-closed startup), B-012 (cross-project mismatch audited, tick survives); source poll errors keep today's per-source error handling |
| Authorization | covered: B-005 (`author_trust_class` passthrough), B-010 (no new external calls/credentials) |
| Concurrency | covered: B-007/B-008 (dedupe and cap checked against store state at dispatch; per-repo parallel dispatch unchanged) |
| Retry + idempotency | covered: B-007 (re-polled issues suppressed while live), B-008 (cap skips retried naturally on later ticks) |
| Illegal transitions | N/A — creation only; instance lifecycle unchanged (B-011) |
| Compatibility | covered: B-001, B-006 (unmatched path untouched), B-003 (schema future-proof for new sources) |
| Degradation / fallback | covered: B-008/B-009 (skips audited, never silent), B-012 |
| Evidence / audit | covered: B-004 (tie reasons), B-009 (route/suppress/skip records) |
| Cancellation / partial | covered: B-011 (cancel semantics inherited); terminal instance reopens intake for the same external id (B-007) |

Cross-product boundary called out explicitly: the same external id
arriving from two different sources is two distinct dedupe keys only if
the definitions differ — within one binding, `external_id` uniqueness is
the source's contract (B-003 + B-007); and a cap-skipped issue must
remain eligible on the next tick even though its remote fact snapshot was
already recorded (B-008 + existing coverage-gate semantics).
