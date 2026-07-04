# Product Spec

## Linked Issue

GH-1448

## User Problem

Every workflow run on a repo starts with zero knowledge of previous runs.
Validation commands that mattered, recurring pitfalls, and reviewer feedback
patterns are rediscovered (and re-billed) each time. Frontier agents ship
repo-scoped memory; harness runs cold forever.

## Goals

- Persist a small structured memory record per terminal workflow run, keyed
  by repo, covering both successes and failures.
- Inject a bounded, relevance-ranked memory section into the activity prompt
  packet at dispatch time.
- Keep memory inspectable and prunable by the operator.

## Non-Goals

- Vector database or embedding retrieval.
- Cross-repo or global memory sharing.
- Mounting memory on the skills/rules layer (GH-1434 Phase 3 removes it).
- Letting memory content act as instructions or override policy.

## Behavior Invariants

1. Every terminal run (done or failed) produces at most one memory record
   with an explicit outcome flag; extraction failure never blocks run
   completion but is logged at error level.
2. Memory records contain only structured fields (validation commands,
   failure class, feedback pattern, evidence refs) — never raw agent output
   dumps.
3. At dispatch, at most K records (default 5) within a hard token budget are
   injected, ranked by recency and outcome relevance; fresh repos get an
   empty section, not filler.
4. Injected memory is delimited as untrusted background evidence; prompt text
   states it must not override task instructions or policy.
5. Failure lessons are retrievable: retrieval never filters to success-only
   records when failures exist for the same activity class.
6. Operator can list memories per repo and delete any record; deletion takes
   effect on the next dispatch.
7. Memory storage lives in existing workflow runtime storage; no new
   storage namespace.

## Acceptance Criteria

- [ ] Done and failed runs both produce outcome-flagged records.
- [ ] Prompt packets show a bounded memory section for repos with history.
- [ ] Retrieval surfaces failure lessons alongside success patterns (test).
- [ ] List + prune API works; pruned records stop appearing in packets.

## Edge Cases

- Run with no extractable signal — no record written (absence is valid, not
  an empty record).
- Memory store unavailable at dispatch — dispatch proceeds memory-less and
  the degradation is surfaced in run evidence (no silent skip).
- Record referencing a validation command that no longer exists in the repo —
  ranked down by staleness; never blocks dispatch.
- Token budget smaller than one record — section omitted with a recorded
  reason.

## Rollout Notes

Feature-flagged per project (`memory.enabled`); default off for one release,
then default on for projects with ≥10 terminal runs. No migration needed.
