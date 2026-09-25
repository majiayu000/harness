# Product Spec

## Linked Issue

GH-1768

## User Problem

Harness ships a deterministic PR-repair scorecard, a replay benchmark
manifest, isolated eval dispatch, and pass@1/pass^k reporting — but no way to
run them end to end. `harness eval run` refuses live execution and requires
hand-collected evidence, no CI job ever executes the suite, no baseline report
exists to diff against, and no eval outcome reaches skill governance or GC.

Operators changing prompts, routing, gates, or context assembly have no
mechanical answer to "did this make the fleet better or worse?" Every quality
claim rests on anecdote, which the 2026-07 session audit showed to be
unreliable at scale.

## Goals

- Make `harness eval run` execute manifest cases end to end without external
  evidence collection.
- Establish committed, reviewable baseline reports and a deterministic
  regression diff against them.
- Run the suite on a schedule in CI with a loud failure on regression.
- Emit typed eval-outcome events into the observe stream so downstream
  consumers (skill governance, GC signals) can attribute measured quality.
- Restore the GH1454 scoped-CI case with a versioned, evaluator-owned verifier
  that runs against its historical candidate workspace.

## Non-Goals

- Authoring new benchmark cases or changing the existing manifest schema
  beyond what execution requires.
- Integrating external benchmarks (SWE-bench proper, Terminal-Bench).
- Gating PR merges on eval results (nightly gate is advisory-then-blocking for
  the eval job itself, not for feature merges).
- LLM-judge scoring; the rubric remains the deterministic
  `score_pr_repair_eval` scorecard.
- Redesigning the scoring rubric, hard gates, or grade caps.
- Cross-provider model comparison dashboards.

## User-Visible Behavior

1. **B-001:** `harness eval run --manifest <path> --execute` runs each case
   end to end: shadow workspace at the pinned `base_commit`, dispatch through
   the existing eval workflow path (`harness-eval/` branch prefix, draft PR
   mode), evidence collection, scoring, and report emission — with no
   `--evidence` input required.
2. **B-002:** The existing `--evidence` and `--dry-run` modes are unchanged;
   `--execute` is mutually exclusive with both and its absence preserves
   today's behavior exactly.
3. **B-003:** Evidence for an executed case is collected by the evaluator
   process (workflow terminal state, server-side PR snapshot, verify-command
   exit codes and output digests). No field of the scored evidence is copied
   from agent self-reported success claims.
4. **B-004:** Each executed case ends in exactly one terminal evidence status
   (passed, failed, timed out, dispatch-failed, evidence-incomplete);
   timeouts and dispatch failures score as failures, never as skips.
5. **B-005:** A completed run writes an `EvalRunReport` JSON artifact whose
   metrics include `pass_at_1` and `pass_to_k`, plus per-case grades, failed
   hard gates, and usage totals.
6. **B-006:** `harness eval diff --baseline <report> --candidate <report>`
   exits nonzero when `pass_at_1` or `pass_to_k` drops by more than a
   configured threshold, or when any case that passed in the baseline newly
   fails an F-cap hard gate. The diff output names each regressing case and
   gate.
7. **B-007:** Baseline reports live in-repo under `evals/baselines/<suite>/`
   (a `latest.json` plus dated history). Updating a baseline is an ordinary
   reviewed change, never a silent side effect of a run.
8. **B-008:** A scheduled CI workflow executes the suite, compares against the
   committed baseline, uploads the candidate report as an artifact, and fails
   the job on regression per B-006. Baseline refresh is proposed as a PR, not
   pushed directly.
9. **B-009:** Every scored case emits one `eval_case_scored` event into the
   observe event stream carrying suite, case id, run id, grade, failed gates,
   and usage. Every completed run emits one `eval_run_completed` event with
   the run metrics. Event emission failure fails the run loudly rather than
   silently dropping the record.
10. **B-010:** Each case run records its token/usage totals in evidence, and a
    suite-level usage ceiling aborts remaining cases with an explicit
    `budget-exhausted` status when exceeded — never reported as passed or
    silently truncated.
11. **B-011:** Eval traffic remains isolated: branches under the eval prefix,
    PRs in draft mode, and no writes to non-eval branches. A case observed
    escaping isolation fails its `BranchSafety`/`NoUnrelatedPrCreation` gates
    as today.
12. **B-012:** Re-running `harness eval run` with the same manifest and run id
    refuses to overwrite an existing completed report for that run id.
13. **B-013:** The GH1454 scoped-CI benchmark is restored only when its
    verifier is evaluator-owned, versioned, executed against the pinned
    candidate workspace, and excluded from the agent prompt. The accepted
    gold patch must pass it and the pinned base must fail it.

## Acceptance Criteria

- [ ] CLI test: `--execute` with `--evidence` or `--dry-run` is rejected;
      absent `--execute` preserves current formatter behavior byte-for-byte on
      existing fixtures.
- [ ] Integration test (DB-backed): a manifest case dispatched via `--execute`
      reaches a terminal state and produces evidence with verify-command exit
      codes and output digests recorded by the evaluator.
- [ ] Unit tests: each terminal evidence status (timeout, dispatch failure,
      incomplete evidence) maps to a failing case, never a skip.
- [ ] Report test: an executed run's report round-trips through
      `diff_eval_run_reports` against a committed baseline fixture.
- [ ] Diff-gate tests: threshold breach on `pass_at_1`, on `pass_to_k`, and a
      newly failing F-cap gate each produce a nonzero exit and name the case.
- [ ] CI workflow present, schedule-triggered, comparing against
      `evals/baselines/`, failing on regression, uploading the candidate
      report artifact.
- [ ] Observe test: scoring a case emits `eval_case_scored`; completing a run
      emits `eval_run_completed`; an event-store write failure fails the run.
- [ ] Budget test: exceeding the suite usage ceiling marks remaining cases
      `budget-exhausted` and the run as failed.
- [ ] Idempotency test: duplicate run id against an existing completed report
      is refused without modifying the original.
- [ ] Historical verifier test: `gh1454_ci_contract_v1` fails the pinned
      GH1454 base, passes its accepted gold patch, runs from evaluator-owned
      bytes rather than the candidate checkout, and records its verifier
      digest in evidence.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-002 and B-012; missing manifest/evidence errors are unchanged existing behavior. |
| Error and failure paths | Covered by B-004, B-009, B-010; every failure mode is a scored failure or a loud run failure. |
| Authorization / permission | Eval dispatch reuses existing runtime submission authority; CI baseline refresh goes through PR review (B-008). |
| Concurrency / race / ordering | Covered by B-012 (run-id uniqueness); per-case dispatch reuses existing runtime leasing. |
| Retry / repetition / idempotency | Covered by B-012; re-scoring from saved evidence remains available via `--evidence`. |
| Illegal state transitions | Cases inherit the runtime's transition validation; evidence-incomplete is an explicit terminal status (B-004). |
| Compatibility / migration | Covered by B-002; no manifest schema break, no change to existing report consumers. |
| Degradation / fallback | Covered by B-004, B-009, B-010; no silent skip, no estimate presented as confirmed. |
| Evidence and audit integrity | Covered by B-003, B-007; evaluator-collected evidence, reviewed baselines. |
| Cancellation / interruption / partial completion | Covered by B-010; an interrupted run leaves per-case evidence with explicit non-passed statuses. |

## Edge Cases

- A case's `base_commit` no longer exists on the remote (history rewrite).
- The verify command passes but the workflow ended `failed` (or vice versa).
- Two cases target the same repo concurrently and contend for workspaces.
- The scheduled CI run starts while a previous scheduled run is still active.
- A regression is caused by a benchmark-environment change (toolchain bump),
  not a harness change — the diff output must make the failing verify command
  and digest visible enough to diagnose.
- The observe event store is unavailable mid-run.

## Rollout Notes

Phase the gate: first scheduled runs publish reports without failing
(collect variance), then enable the regression threshold. Seed
`evals/baselines/<suite>/latest.json` from the first green full run via a
reviewed PR. No migration; all new surfaces are additive. Reverting removes
the CI job and CLI flag without affecting existing evidence-based reporting.
