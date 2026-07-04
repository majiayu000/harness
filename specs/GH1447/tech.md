# Tech Spec

## Linked Issue

GH-1447

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Workflow runtime | `crates/harness-workflow/src/runtime/` (dispatcher, worker, reducer, submission, quality_gate) | Dispatches issue/PR activities, persists runtime_events/jobs/workflows | Eval cases must ride this exact path |
| Scoring semantics | `crates/harness-eval/src/scoring.rs` (orphaned, deletion pending GH-1434) | Deterministic PR-repair scoring with hard gates | Port the pure functions; do not link the crate |
| Quality gate | `crates/harness-workflow/src/runtime/quality_gate.rs` | Scores runtime submissions | Evidence source for case scoring |
| Usage accounting | `crates/harness-server/src/handlers/usage_monitor*.rs`, `crates/harness-observe/src/usage.rs` | Token/cost attribution per process | Cost/tokens per case |
| Manual eval artifacts | `docs/pr-repair-evals/`, `docs/pr-repair-capability-evaluation.md` | Hand-run eval records | Seed data for the benchmark manifest |

## Proposed Design

1. **Benchmark manifest** — `evals/benchmarks/<name>.toml`: list of cases
   `{repo, issue, base_commit, verify_commands, timeout_secs}`. Checked into
   the repo; the initial set curates 10–20 merged harness issues.
2. **Eval driver** — a thin module in `harness-workflow` (`runtime/eval_run.rs`)
   that, for each case, creates a standard workflow with an `eval_run_id`
   marker in existing metadata columns, pinned to `base_commit`.
3. **Evidence collection** — reuse runtime submission + quality_gate records;
   an eval case is scored from: validation command exit codes, CI conclusion,
   diff stats, and submission state at terminal.
4. **Scorer** — pure functions ported from `harness-eval/src/scoring.rs`
   (hard gates + breakdown) into the eval module, with a determinism unit test.
5. **Report** — `harness eval run --manifest <path> --k 3` prints and persists
   a JSON report (pass@1, pass^k, per-case evidence refs, cost, tokens);
   `harness eval diff <run_a> <run_b>` renders transitions.
6. **Safety** — eval workflows carry a distinct branch prefix
   (`harness-eval/`), PR titles are prefixed `[eval]`, PRs are opened as
   drafts and closed (not merged) at scoring time.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P2 same dispatch path | eval_run.rs uses existing dispatcher API | integration test asserting identical activity records |
| P4 deterministic scoring | ported scorer pure fns | unit test: same evidence → identical bytes |
| P5 metric completeness | report builder | unit test with missing-evidence fixture → case fails |
| P8 no orphans | reuse workspace cleanup + reaper | integration test: cancel mid-run, assert zero leftovers |

## Data Flow

Manifest (TOML, repo) → eval driver → workflow runtime (Postgres, existing
schemas, `eval_run_id` metadata) → GitHub (draft PRs, CI) → evidence rows →
scorer → report JSON (persisted as runtime record + stdout).

## Alternatives Considered

- Keep harness-eval as a crate and wire it in — rejected: GH-1434 approved
  deletion; a standalone layer already failed once with 0 runs.
- LLM-judge scoring — rejected as primary signal (selector failure modes);
  may be revisited as an annotator, never a gate.
- External eval service (Langfuse/Braintrust datasets) — deferred; internal
  loop must exist first, export later via GH-1451 spans.

## Risks

- Security: eval runs execute agent code on historical issues — same trust
  model as production runs; draft PRs prevent accidental merges.
- Compatibility: depends on GH-1434 ordering; port scorer before crate delete.
- Performance: N cases × agent runs is expensive — per-case and per-run budget
  caps mandatory; k>1 multiplies cost, default k applies only when requested.
- Maintenance: benchmark rot as the repo evolves — pinned base commits plus an
  `infra_failed` class keep reports honest; refresh cadence documented.

## Test Plan

- [ ] Unit tests: scorer determinism, report math (pass@1/pass^k), manifest parsing.
- [ ] Integration tests: two-case manifest against fixture repo; cancel-mid-run cleanup.
- [ ] Manual verification: full run of the curated set on the live harness; diff two runs.

## Rollback Plan

The eval driver is additive and invoked only by explicit command; disable by
removing the CLI/API entry. Runtime records use existing schemas, so rollback
requires no migration.
