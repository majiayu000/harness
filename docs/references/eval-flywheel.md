# Eval Flywheel — Analysis Report

> Linked issue: GH-1768
> Date: 2026-07-26
> Scope: why the existing eval infrastructure produces no feedback loop, and what
> is required to close it. Read-only analysis; the companion spec packet is
> `specs/GH1768/`.

## Executive Summary

Harness already contains the hard 80% of a state-of-the-art evaluation system:
a deterministic PR-repair scorecard with fail-closed hard gates, a
SWE-bench-style replay manifest with per-case verify commands, isolated
dispatch through the production runtime, and pass@1 / pass^k reporting with
baseline diffing. None of it is connected to anything. The scorer has no
caller outside the CLI, the CLI cannot execute a case ("live eval execution is
not wired to the CLI yet", `crates/harness-cli/src/commands/eval.rs:82`), no CI
workflow runs evals, no baseline report is committed, and no eval outcome ever
reaches skill governance, GC signals, or prompt selection.

The consequence is that every change to prompts, routing, gates, and context
assembly ships judged by anecdote. The difference between an orchestrator and
a self-improving system is exactly this loop. Closing it is mostly plumbing,
not research.

## 1. What Exists (verified 2026-07-26)

### 1.1 Deterministic scoring — `crates/harness-workflow/src/runtime/eval/scoring.rs` (449 lines)

`score_pr_repair_eval(PrRepairEvalInput) -> QualitySnapshot` is a pure
function over collected evidence. It computes eleven hard gates
(`scoring.rs:44-110`), each of which caps the final grade via
`most_restrictive_cap` (`scoring.rs:126-127`):

| Gate | Cap on failure | Meaning |
|---|---|---|
| `TargetCorrectness` | F | final PR matches the requested target |
| `BranchSafety` | F | base/head refs stayed on the requested PR |
| `NoUnrelatedPrCreation` | F | run did not create an unrelated PR |
| `ScopeContainment` | F | diff stayed within repair scope |
| head-change gate (scenario-dependent) | — | baseline `head_oid` ≠ final `head_oid` (`scoring.rs:30`) |
| `HeadFreshness` | C | final evidence collected for the final head (`scoring.rs:31-34`) |
| `RequiredChecks` | C | checks passing on final head |
| `MergeabilityClean` | C | mergeability clean |
| `ReviewThreadClosure` | C | threads fully enumerated, none unresolved |
| `RuntimeArtifactCompleteness` | B | usable runtime task/workflow/job artifact present |
| reviewer gate | — | reviewer evidence consistency |

Two properties are worth calling out because they are exactly what most
LLM-judge eval stacks lack:

- **"Did anything actually change" is a first-class check** — `head_changed`
  and `final_evidence_fresh` make a do-nothing run or a stale-evidence run
  unable to score well.
- **Grades are capped, not averaged** — a run that fabricates success on one
  axis cannot buy the grade back on the others.

`scoring_tests.rs` (491 lines) pins the rubric.

### 1.2 Replay manifest — `eval/manifest.rs` + `evals/benchmarks/harness-core.toml`

The manifest defines a suite of real, historical harness issues replayed from
pinned commits, each with objective verification:

```toml
[[cases]]
case_id = "gh1437-runtime-stall-detection"
repo = "majiayu000/harness"
issue = 1437
base_commit = "8b3a328375178f98d3aac0865d49fc1bf81c869e"
verify_commands = ["cargo test -p harness-server stall"]
```

14+ cases, `default_timeout_secs = 7200`. This is the same shape as
SWE-bench-verified: frozen base commit, real issue, deterministic verify
command. The corpus regenerates itself for free — every merged fix with a
driving issue is a candidate case.

### 1.3 Isolated dispatch through the production runtime — `eval/run.rs` (768 lines)

`enqueue_eval_case_workflow` / `dispatch_eval_case_workflow`
(`run.rs:122,207`) submit eval cases through the normal workflow runtime with
`EVAL_BRANCH_PREFIX = "harness-eval/"` and `EVAL_PR_DRAFT_MODE = "draft"`
(`run.rs:12-13`) — shadow branches and draft PRs, so eval traffic exercises
the real pipeline without touching production branches. This matches the
isolation model that `docs/parallel-pr-repair-bakeoff-spec.md` asks for.

### 1.4 Reporting — `eval/report.rs` (399 lines)

`EvalRunMetrics { pass_at_1, pass_to_k, ... }` (`report.rs:24-25`),
per-case transitions, and `diff_eval_run_reports` producing
`pass_at_1_delta` / `pass_to_k_delta` against a baseline report
(`report.rs:230-231`). The regression-diff primitive already exists, and it is
already exposed on the CLI: `harness eval diff --baseline --candidate`
(`commands/eval.rs::diff_eval_reports`, ~line 89) validates suite/k
compatibility and emits the diff — but always exits 0; there is no threshold
gate and no failing exit code on regression.

### 1.5 Evidence model — `eval/evidence.rs` (610 lines), `eval/model.rs`

Typed `EvalCaseEvidence` with status, confidence (`Confidence` enum:
`Exact`, `Estimated`, `Observed`, `Unknown` — `model.rs:137`), usage
snapshots, and remote-fact capture.

## 2. The Disconnection (verified)

1. **No runtime consumer.** The only references to `score_pr_repair_eval`
   outside the eval module are the re-export in `runtime/mod.rs` and
   `harness-cli/src/commands/eval.rs`. Nothing in `harness-server` consults an
   eval score for any decision.
2. **The CLI cannot execute a case.** `harness eval run` accepts `--manifest`
   plus either `--dry-run` (validate manifest) or `--evidence <json>`
   (pre-collected). With neither it bails: *"live eval execution is not wired
   to the CLI yet; pass --evidence to report collected evidence or --dry-run
   to validate the manifest"* (`commands/eval.rs:82`). Evidence collection —
   the actual running of agents against cases — is external and manual.
3. **No CI wiring.** `.github/workflows/` contains no job invoking
   `harness eval`; the only grep hit for "eval" across workflows is unrelated
   (`workflow-check.yml`). There is no nightly run, no PR-triggered run.
4. **No committed baselines.** `evals/` contains only `benchmarks/`. No
   `evals/baselines/` directory, no historical report to feed
   `diff_eval_run_reports`. The diffing code has never had two real inputs.
5. **Prior eval attempts were manual one-offs.** The `docs/pr-repair-evals/`
   artifacts (uncommitted working-tree material) are hand-written scorecards
   from individual Codex sessions; one records that the evaluator script
   itself was broken (empty `task_detail_final.json` from unencoded task-id
   polling). Two of four runs were aborted. This is what eval-by-hand
   converges to.
6. **Scores feed nothing.** Skill governance maintains an EMA
   `quality_score` with Active/Watch/Quarantine/Retired states
   (`harness-skills/src/store.rs:37,53-54`) — updated from usage outcomes,
   never from eval results. GC's `SignalDetector` consumes observe events —
   no eval events exist to consume. Prompt/template selection has no notion
   of measured quality at all.

## 3. Why This Is the Highest-Leverage Gap

Harness's own operational history is the argument:

- The 2026-07 audit of 132 autonomous sessions found "false done" claims,
  spec busywork (~24 PRs, 1 real implementation), and review-gate driftage —
  all failure modes a replayed benchmark with hard gates detects mechanically.
- The runtime has since grown strong per-run defenses (zero-output detection,
  status-contract downgrade, server-owned PR snapshots). Those verify a
  *single run*. Nothing verifies the *system*: after a prompt rewrite, a
  router change, or a context-composer rollout, there is no number that says
  whether harness got better or worse at its actual job.
- Every mature harness effort (internal SWE-bench-style suites at the model
  labs, Codex's own eval gating) converges on the same loop:
  **change → replay suite → pass^k diff → accept/revert**. Harness has every
  component of this loop built and none of the edges.

The flywheel, once closed, also compounds the other improvement tracks:
semantic retrieval (GH-1769), context-composer enforcement, and prompt changes
all become measurable experiments instead of vibes.

## 4. Target Design

### 4.1 Self-driving evidence collection

`harness eval run --manifest ... --execute` performs, per case, in-process:
prepare shadow workspace at `base_commit` → submit through
`dispatch_eval_case_workflow` (existing) → await terminal state → collect
evidence (PR snapshot, runtime artifacts, verify-command results) into
`EvalCaseEvidence` → score. The manual `--evidence` path remains for offline
re-scoring. Verify commands run server-side with recorded exit codes and
output digests — evidence is collected by the evaluator, never self-reported
by the evaluated agent.

### 4.2 Nightly CI run + committed baselines

A scheduled workflow executes the suite against pinned base commits on a
runner with an agent runtime, then uploads the `EvalRunReport` and opens a PR
updating `evals/baselines/<suite>/latest.json` (plus a dated history file).
Baselines live in-repo so `diff_eval_run_reports` has a canonical input and
report changes are themselves reviewable.

### 4.3 Regression gate

`harness eval diff --baseline ... --candidate ... --max-pass-drop <t>` exits
nonzero when `pass_at_1_delta` or `pass_to_k_delta` falls below the threshold
or any previously-passing case newly fails a hard gate at F-cap severity. The
nightly job fails loudly on regression; an optional PR-labelled job lets
high-risk changes (prompt templates, routing, context assembly) request a
pre-merge eval.

### 4.4 Outcome feedback into the observe stream

Each scored case emits a typed observe event (`eval_case_scored`: suite,
case_id, grade, failed gates, usage) into the existing EventStore. GC's
SignalDetector gains an eval-regression signal; skill governance can
attribute eval outcomes to skills that were injected into the run's prompts,
finally grounding the EMA `quality_score` in measured task success.

## 5. Non-Goals (this iteration)

- Authoring new benchmark cases (corpus growth is continuous, separate work).
- External benchmark integration (SWE-bench proper, Terminal-Bench).
- Blocking merges on eval results — the gate starts advisory in CI; merge
  gating is a policy decision for after the suite proves stable.
- LLM-judge scoring — the rubric stays deterministic.

## 6. Risks

- **Cost**: a full nightly suite is 14+ agent runs. Mitigations: per-case
  usage budgets recorded in evidence, suite-level USD ceiling (aligns with
  GH-1770), and k=1 nightly with k>1 weekly.
- **Flake**: verify commands depend on toolchain determinism; pinned
  base commits and recorded command output digests keep failures diagnosable.
- **Benchmark overfitting**: cases derive from harness's own history; the
  corpus must keep growing from newly merged fixes to stay representative.

## 7. Related Documents

- `specs/GH1768/product.md`, `specs/GH1768/tech.md` — the spec packet.
- `docs/parallel-pr-repair-bakeoff-spec.md` — draft bakeoff design sharing the
  shadow-branch isolation model.
- `docs/references/server-verified-completion.md` (GH-1766) — the trust
  boundary evals depend on.
- `docs/references/budget-enforcement.md` (GH-1770) — cost ceilings for eval
  traffic.
