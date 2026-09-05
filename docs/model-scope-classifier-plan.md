# PR Scope Guard: Deferred Integration

> Status: Deferred proposal, not an implementation queue.
> Reconciled against main `121df4e4` on 2026-09-05.

## Delivered

The generic semantic activity primitive is already delivered through
`WorkflowAgentContract` in #2020, #2025, #2028, and #2031. It provides pinned
inputs and provenance, no-tool execution, structured verdict validation,
server-authored assessments, routing, and durable usage accounting. #2033
repairs real-execution error propagation, retry, stall, and recovery paths.

The controlled Codex run in
[the Slice D report](features/8-31/agent-contract-slice-d-production-dogfood.md)
completed and survived restart without another model call. It did not measure
long-running reliability or USD spend: `cost_usd_observed` was false.

Reuse this path if a PR scope guard is approved. Do not restore PR #2008's
parallel classifier configuration, execution driver, or assessment machinery.

## Remaining product question

A PR scope guard would compare the complete current PR change with the requested
issue and return a semantic assessment. That GitHub integration is not delivered
by the generic contract primitive. Before implementation, establish a concrete
case where it improves current supervised work and agree on the required facts
and operator response.

The smallest candidate uses an ordinary agent activity to collect the facts and
passes one provenance-covered snapshot to the existing contract activity. The
model makes the scope judgment; file counts and diff size do not decide it.
Collector output stays agent-authored and untrusted on reinjection. A successful
semantic assessment alone is not independent verification of GitHub facts or
merge authorization.

Any future collector must have technically enforced read-only access; an
instruction in its prompt does not establish that boundary. Its design must
cover complete diff collection, missing patches, pagination, and changes to the
issue, PR metadata, base, or head during collection. Existing local review and
remote readiness gates remain applicable.

## Decision and boundaries

PR #2008 is closed as superseded, with its branch preserved for reference.
There is no approved follow-up implementation series. In particular, this note
does not require a multi-version built-in registry, historical-row compatibility,
a `github_issue_pr@2` rollout, merge leases, active merge cancellation, or vNext.
Those are separate choices that require a demonstrated current need.

The immediate queue is structured PR repair outcomes, live evaluation in #1768,
and observed cost plus verified budget limits in #1770. Scope-guard integration
remains deferred until the owner selects it after that operating evidence.

If selected, define one bounded acceptance case before editing code, retain
operator-owned recovery, use existing contract failure semantics, and validate
through a real Harness runtime submission. Do not cherry-pick the old branch
wholesale or add a second generic semantic execution path.
