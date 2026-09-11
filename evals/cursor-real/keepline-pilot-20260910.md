# Keepline real Cursor pilot — 2026-09-10

Verdict: **incomplete; do not count as an autonomous end-to-end pass**.

This hard-task follow-up used frozen Keepline PR #112 commit
`a2bd28daabb5475ff083c03829709410b2970d9d` and the real requirements of Issue #111.
All four coding/review activities ran through Harness with real Cursor sessions.
No candidate changes were authored by the evaluator, pushed, or merged.

## Observed sequence

| Stage | Actual activity duration | Outcome |
|---|---|---|
| Implementation | 724.8 seconds | Reported completion; later rejected by review |
| Independent review | 492.3 seconds | Three blocking findings |
| Repair after feedback | 250.4 seconds | Addressed findings and removed impersonation test |
| Fresh independent re-review | 191.6 seconds | Found no blockers for tested subset; full requirement still unverified |

Approximately 28.6 minutes elapsed from first activity creation to last completion,
including the evaluator's resubmission gap. Cursor model selection was `auto`;
actual cost and token use are unknown. Different runtime thread IDs are recorded
in the local run evidence.

## Findings the evaluation exposed

1. New daemon tests reached the operator's actual provider roots through module-load
   defaults, causing roughly 42,816 session records to be scanned into disposable
   test data and about 100-second scans. Default-timeout tests failed. This was a
   data-isolation failure, not useful extra reasoning. We did not inspect session
   contents during evaluation. No modification of the operator's source session
   files was established; the incidental scan itself was outside the intended scope.
2. Ownership retry tests could appear green beside a timing-out sibling yet fail
   independently. A reported passing suite was not reproducible by the reviewer.
3. Service Mode consumed its initial full-scan flag before the child succeeded;
   failure could prevent the required full retry.
4. The first implementation used a disposable process named like Claude and a
   generated transcript as restoration evidence. That violates this suite's
   genuine-Agent requirement. This evidence was rejected, and the added
   impersonation test was removed in round 2. The run must not be described as
   having used no simulation merely because the coding agents themselves were real.

Round 2 isolated session scan roots, repaired the scan flag lifecycle, and made
ownership tests run against real local service binding and disposable storage
without mocking sync or increasing timeouts. The evaluator independently reran
real daemon start/stop and bind-failure checks (two passing tests, 3.8 seconds)
and TypeScript checking (exit 0). These daemon checks use controlled database
records, not genuine provider sessions, and do not prove live Agent restoration.

## Why the full case remains incomplete

- Genuine Claude/Codex live-session restoration after restart was not exercised.
  The reviewer acknowledges this; partial checks cannot satisfy the full acceptance.
- The re-review file contains APPROVED followed by an untested-requirement note.
  Its final non-empty line is not APPROVED. Under the repository's direct-review
  contract, this is not machine-parseable approval; we did not rewrite it into one.
- Prompt-task execution states are all `done`, including the first review that
  correctly found blockers. Activity completion is not candidate correctness.
- The evaluator read round 1 and submitted round 2 through Harness. Each paired
  review was dependency-scheduled automatically, but native review-to-repair
  routing was not exercised. This is an operator-assisted pilot, not evidence
  that the production PR repair loop or its quality gate is fixed.
- No genuine held-out set, paired prompt variant, 20-worker batch, or original
  baseline behavior reproduction was completed. This is one development task.

## Evidence

Run directory: `/Users/apple/.local/share/harness/cursor-evals/20260910-143502`.

- `evidence/runtime-jobs.json`: all four real job outcomes, thread identifiers,
  transcript references and checksums.
- `evidence/round1-EVAL_HANDOFF.md` / `round1-EVAL_REVIEW.md`: original claims
  and rejection, preserved before further changes.
- `keepline/EVAL_HANDOFF.md` / `EVAL_REVIEW_ROUND2.md`: repair handoff and review.
- `evidence/evaluator-checks.json` and logs: fresh independent checks.
- `evidence/candidate-files/`, `candidate.patch`, and file hashes: exact final
  uncommitted candidate, including new files.
- Submitted prompts and dependency handles are saved per stage.

The local partial-clone fetch initially failed; the same pinned SHA was fetched
from GitHub successfully. This setup failure is separate from candidate quality.

## Implication

Short runtime is not automatically poor work, and long runtime is not proof of
quality. This harder case surfaced faulty evidence, real code defects, isolation
failure, incomplete acceptance, and an output-contract error. Preserve these
outcomes rather than converting partial review approval into a benchmark pass.
