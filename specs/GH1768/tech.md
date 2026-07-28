# Tech Spec

## Linked Issue

GH-1768

## Product Spec

`specs/GH1768/product.md`

## Context

All building blocks exist in `crates/harness-workflow/src/runtime/eval/`:

- `scoring.rs` — `score_pr_repair_eval` with eleven hard gates and grade caps
  (`most_restrictive_cap`).
- `manifest.rs` — `EvalBenchmarkManifest` parsed from
  `evals/benchmarks/<suite>.toml` (`case_id`, `repo`, `issue`, `base_commit`,
  `verify_commands`, `default_timeout_secs`).
- `run.rs` — `enqueue_eval_case_workflow` / `dispatch_eval_case_workflow`
  submitting through the production runtime with
  `EVAL_BRANCH_PREFIX = "harness-eval/"`, `EVAL_PR_DRAFT_MODE = "draft"`.
- `evidence.rs` / `model.rs` — typed `EvalCaseEvidence`, `Confidence`,
  usage snapshots.
- `report.rs` — `EvalRunReport`, `EvalRunMetrics { pass_at_1, pass_to_k }`,
  `diff_eval_run_reports` with `pass_at_1_delta` / `pass_to_k_delta`.
- `harness eval diff` already exists on the CLI
  (`harness-cli/src/commands/eval.rs::diff_eval_reports`): takes
  `--baseline`/`--candidate`, validates suite and k compatibility, and emits
  the diff — but always exits 0, with no regression threshold.

The gap is orchestration: `harness-cli/src/commands/eval.rs:82` bails on live
execution; nothing collects evidence in-process; no CI job or baseline exists;
no observe events are emitted.

## Design Overview

Four additive components, no changes to scoring or the manifest schema:

```
┌ CLI: harness eval run --execute ──────────────────────────────┐
│  EvalExecutor (new, harness-workflow::runtime::eval::execute) │
│    for each case (bounded concurrency, suite usage ceiling):  │
│      1 dispatch_eval_case_workflow (existing)                 │
│      2 await terminal state (poll store, case timeout)        │
│      3 collect_case_evidence (new): PR snapshot via server    │
│        GraphQL collector, runtime artifacts, verify commands  │
│        with exit codes + sha256 output digests                │
│      4 score_pr_repair_eval (existing)                        │
│      5 emit eval_case_scored (observe EventStore)             │
│  write EvalRunReport → emit eval_run_completed                │
└───────────────────────────────────────────────────────────────┘
   harness eval diff --baseline evals/baselines/<suite>/latest.json
       --candidate <report> --max-pass-drop <t>   → exit code gate
   .github/workflows/eval-nightly.yml → schedule → run + diff + artifact
```

## Component Changes

### 1. `EvalExecutor` — new module `runtime/eval/execute.rs` (harness-workflow)

```rust
pub struct EvalExecuteConfig {
    pub run_id: String,
    pub k: u32,                       // attempts per case, default 1
    pub case_timeout: Duration,       // manifest default_timeout_secs override
    pub max_concurrent_cases: usize,  // default 1
    pub suite_usage_ceiling: Option<UsageCeiling>,
}

pub async fn execute_manifest(
    store: &WorkflowRuntimeStore,
    runtime_profile: RuntimeProfile,
    observe: &EventStore,
    manifest: &EvalBenchmarkManifest,
    cfg: EvalExecuteConfig,
) -> Result<EvalRunReport, EvalExecuteError>;
```

`store` and `runtime_profile` match the existing concrete signature of
`dispatch_eval_case_workflow(&WorkflowRuntimeStore, RuntimeProfile, ...)`
(`run.rs:207`); `observe` is the concrete `harness_observe::EventStore` — no
new trait abstraction is introduced by this spec.

- Per case: `dispatch_eval_case_workflow` → poll instance until a terminal
  state or `case_timeout` → build `EvalCaseEvidence`.
- Terminal mapping (product B-004): workflow `failed`/`cancelled`, dispatch
  error, timeout, and incomplete evidence each map to a distinct
  `EvalEvidenceStatus` variant that scores as a failed case. No skip path.
- Run-id uniqueness (B-012): `execute_manifest` refuses to start when a
  completed report artifact for `run_id` already exists at the output path.
- Suite ceiling (B-010): accumulated usage from case evidence is checked
  before each dispatch; on breach, remaining cases get status
  `BudgetExhausted` and the run result is an error carrying the partial
  report.

### 2. Evidence collection — new `runtime/eval/execute/collect.rs`

- **PR snapshot**: reuse the server-owned GraphQL snapshot path
  (`github_pr_snapshot`-equivalent collector exposed from the runtime), so
  `snapshot_source` on eval evidence matches the production
  `server_github_graphql` provenance. No agent-reported PR state enters
  evidence.
- **Verify commands**: executed by the evaluator in the case's shadow
  workspace via the existing sandboxed command execution used by quality
  validation; record `{command, exit_code, output_sha256, duration_ms}` per
  command. Command text comes only from the committed manifest — never from
  workflow data — so agent-influenced state cannot alter what is verified.
- **Evaluator-owned verifiers**: versioned verifier assets are selected by
  case id in trusted control code and executed against the shadow workspace
  without being copied into that workspace or exposed in the agent prompt.
  `gh1454_ci_contract_v1` is the first required asset; its digest is recorded
  with the command evidence. Restore the GH1454 manifest case only after a
  fixture proves its pinned base fails and accepted gold patch passes.
- **Runtime artifacts**: read `workflow_artifacts` rows for the case instance
  (transcript reference, activity gate artifacts) to populate
  `RuntimeSnapshot` for the `RuntimeArtifactCompleteness` gate.
- Missing/partial collection yields `EvidenceIncomplete` (fail), preserving
  the scorer's fail-closed posture.

### 3. CLI — `harness-cli/src/commands/eval.rs`

- Add `--execute` flag; mutually exclusive with `--evidence` and `--dry-run`
  (extend the existing exclusivity check at `eval.rs:67`).
- `--execute` requires a configured runtime store (same resolution as
  `serve`); absence is a hard error naming the missing configuration.
- Extend the **existing** `eval diff` subcommand (`diff_eval_reports`,
  already accepting `--baseline`/`--candidate` and always exiting 0) with
  `--max-pass-drop <float>` (applies to both `pass_at_1_delta` and
  `pass_to_k_delta`) and `--fail-on-new-f-gate` (default true), adding
  nonzero-exit semantics on breach. Output lists each regressing case with
  the failed gate names. Without the new flags, current exit-0 behavior is
  preserved.

### 4. Observe events — `harness-observe`

Two new event kinds through the existing `EventStore` write path:

- `eval_case_scored { suite, run_id, case_id, grade, failed_gates: Vec<String>, usage }`
- `eval_run_completed { suite, run_id, pass_at_1, pass_to_k, passed_cases, total_cases, usage }`

Emission failure aborts the run (B-009). Consumers (GC `SignalDetector`
eval-regression signal, skill attribution) are follow-up work and explicitly
out of scope here; this spec only guarantees the events exist and are durable.

### 5. CI — `.github/workflows/eval-nightly.yml`

- `schedule` trigger (nightly) + `workflow_dispatch`.
- Steps: build CLI → provision disposable Postgres (existing
  `scripts/dev-db.sh` pattern) → `harness eval run --manifest
  evals/benchmarks/harness-core.toml --execute --run-id nightly-<date>` →
  `harness eval diff --baseline evals/baselines/harness-core/latest.json
  --candidate <report>` → upload candidate report artifact.
- Concurrency group `eval-nightly` with `cancel-in-progress: false` so
  overlapping scheduled runs queue rather than race.
- Baseline refresh: a manual `workflow_dispatch` input `refresh-baseline=true`
  opens a PR (never a direct push) replacing `latest.json` and appending
  `history/<date>.json`.
- Rollout phase 1 runs the diff step with `continue-on-error: true` (variance
  collection); phase 2 removes it.

### 6. Repo layout

- `evals/baselines/<suite>/latest.json` + `evals/baselines/<suite>/history/`
  — committed, reviewed.
- Run outputs default to `artifacts/eval/<run_id>/report.json` locally
  (untracked), uploaded as CI artifacts in the workflow.

## Constraints Honored

- **No `Command::new("gh")`/`Command::new("git")` in harness crates**: shadow
  workspace preparation and PR snapshotting reuse the existing agent-prompt /
  server-collector paths; verify commands are project build/test commands
  executed through the existing sandboxed executor, not git/gh invocations.
- Scoring, manifest schema, and report schema are untouched; all new types are
  additive (`EvalEvidenceStatus` gains variants only if serialization remains
  backward-compatible; otherwise a parallel field is used).
- No `Cargo.toml` version changes.

## Error Handling

| Failure | Behavior |
| --- | --- |
| Dispatch rejected (config invalid, tier unavailable) | Case status `DispatchFailed`, scored fail, run continues. |
| Case timeout | Case status `TimedOut`, scored fail, instance cancellation requested. |
| Evidence collection partial | `EvidenceIncomplete`, scored fail. |
| Observe write failure | Run aborts with error; partial report retained on disk. |
| Suite ceiling breached | Remaining cases `BudgetExhausted`; run exits nonzero with partial report. |
| Existing report for run id | Refused before any dispatch. |

## Test Plan

- `runtime/eval/execute` unit tests with a mock store: terminal-status
  mapping table (every non-passed terminal → failed case), run-id refusal,
  ceiling breach ordering (no dispatch after breach).
- DB-backed integration test (feature-gated like other Postgres suites): one
  synthetic manifest case through dispatch → terminal → evidence → score.
- CLI tests: flag exclusivity matrix; `eval diff` exit codes for pass_at_1
  drop, pass_to_k drop, new F-gate failure, and clean pass.
- Observe tests: event emission on score/complete; abort on sink failure.
- Fixture round-trip: executed-run report diffs cleanly against a committed
  baseline fixture.

## Product-to-Test Mapping

| Product behavior | Test(s) in Test Plan |
| --- | --- |
| B-001 end-to-end `--execute` | DB-backed integration test |
| B-002 mode compatibility | CLI flag-exclusivity matrix; formatter fixture byte-for-byte check |
| B-003 evaluator-collected evidence | Integration test asserting evidence provenance + digests |
| B-004 terminal statuses fail-closed | Terminal-status mapping table tests |
| B-005 report contents | Fixture round-trip test |
| B-006 regression gate | `eval diff` exit-code tests (pass drop, new F-gate, clean) |
| B-007 committed baselines | Fixture round-trip against committed baseline fixture |
| B-008 CI behavior | Workflow file review + phase-1 report-only dry runs |
| B-009 observe events | Event emission + sink-failure abort tests |
| B-010 usage ceiling | Ceiling-breach ordering test |
| B-011 isolation | Existing `BranchSafety`/`NoUnrelatedPrCreation` gate tests (unchanged) |
| B-012 run-id idempotency | Run-id refusal unit test |
| B-013 trusted GH1454 replay | Evaluator-owned verifier provenance/digest test plus pinned-base-fails and gold-patch-passes fixtures |

## Rollout / Revert

Additive only. Phase 1: land executor + CLI + events, run nightly in
report-only mode. Phase 2: seed baseline via reviewed PR, enable the diff
gate. Revert = delete workflow file and `--execute`/`diff` flags; evidence
formatter mode is untouched throughout.

## Rollback Plan

Every component is independently revertible with no data migration:

1. **CI gate misfires** — set the diff step back to `continue-on-error: true`
   (phase-1 posture) without touching code.
2. **Executor defects** — remove `--execute` wiring; `--evidence`/`--dry-run`
   formatter paths are untouched and remain the fallback.
3. **Observe event issues** — the two event kinds are additive; consumers do
   not exist yet, so dropping emission has no downstream effect.
4. **Baseline corruption** — baselines are reviewed in-repo files; revert the
   baseline commit.

No persisted schema changes anywhere; rollback is `git revert` plus CI
workflow deletion at worst.
