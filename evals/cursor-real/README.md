# Real Cursor workflow evaluation

This suite evaluates Harness running genuine Cursor Agent sessions through the
workflow runtime: implementation, independent review, repair, re-review, and
submission. It does not evaluate scripted Agent responses, replay old responses
as new work, or count dry runs as execution evidence. Cursor chooses how to
investigate, implement, and check the work within repository instructions.

## Current deliverable

[catalog.json](catalog.json) contains 15 real candidate tasks with GitHub-verified
starting commits, source links, provisional difficulty, acceptance observations,
and environment requirements. Eight historical Issue cases reuse source metadata
from `../benchmarks/harness-historical-replay.toml`; seven cases use actual PRs.
All 15 began as **source-verified, not calibrated**. A subsequent
[Loom real Cursor pilot](loom-pilot-20260910.md) reproduced and repaired one case
and exercised automatic dependency handoff to independent review. The [full native development run](full-native-20260912.md) now includes all 15
cases and has completed its native workflows and independent assessments.
Missing acceptance evidence, assisted repairs, and CI failures are explicit;
no calibrated suite baseline or strict autonomous pass rate is claimed. This catalog is evaluator-owned planning data, not a file accepted by
`harness eval run --manifest`.

The subsequent [Keepline hard-task pilot](keepline-pilot-20260910.md) ran four
real Cursor activities, found three blockers, and exercised an operator-assisted
repair and re-review. Its result is incomplete: genuine provider restoration
remains untested, initial impersonation evidence was rejected, and the final
review marker does not meet the direct-review output contract. Do not count
these prompt-task `done` states as four successful repairs.

| Case | Difficulty | Source | Main capability |
|---|---|---|---|
| issue-lifecycle | Hard | [Harness #1715](https://github.com/majiayu000/harness/issues/1715) | State correctness |
| startup-recovery | Hard | [Harness #1716](https://github.com/majiayu000/harness/issues/1716) | Recovery and concurrency |
| context-protocol | Medium | [Harness #1717](https://github.com/majiayu000/harness/issues/1717) | Cross-crate refactoring |
| intake-coverage | Hard | [Harness #1707](https://github.com/majiayu000/harness/issues/1707) | Real intake and duplicate prevention |
| durable-transcript | Hard | [Harness #1704](https://github.com/majiayu000/harness/issues/1704) | Transcript durability |
| bun-toolchain | Easy | [Harness #1686](https://github.com/majiayu000/harness/issues/1686) | Focused configuration repair |
| definition-visibility | Medium | [Harness #1652](https://github.com/majiayu000/harness/issues/1652) | Operator visibility |
| intake-routing | Hard | [Harness #1656](https://github.com/majiayu000/harness/issues/1656) | Routing and dispatch |
| techpulse-pr | Hard | [TechPulse #1](https://github.com/majiayu000/techpulse/pull/1) | Broad repair and scope control |
| loom-pr | Easy | [Loom #679](https://github.com/majiayu000/loom/pull/679) | Dependency repair |
| keepline-pr | Hard | [Keepline #112](https://github.com/majiayu000/keepline/pull/112) | Process ownership and restart |
| ccstats-pr | Hard | [CCStats #179](https://github.com/majiayu000/ccstats/pull/179) | Cross-platform correctness |
| operator-snapshot-pr | Medium | [Harness #2047](https://github.com/majiayu000/harness/pull/2047) | Real runtime observation |
| vibeguard-pr | Hard | [VibeGuard #801](https://github.com/majiayu000/vibeguard/pull/801) | Installation and scope control |
| rekey-pr | Hard | [Rekey #48](https://github.com/majiayu000/rekey/pull/48) | Real external-service integration |

Difficulty is an initial judgment, not measured task difficulty. This cohort is
Rust/Harness-heavy, with 2 easy, 3 medium, and 10 hard candidates. It does not
represent all 87 repositories. Add Java/WebGoat, direct Remem tasks, frontend
interaction, and additional easy/medium tasks before broader claims. The Remem
incidents behind two Harness issues do not constitute direct Remem coverage.

These cases have already informed development discussions, so none is an honest
held-out task. Reserve a separate set of previously unused tasks before comparing
prompt or memory changes for generalization. Public historical fixes may also
have appeared in model training; fresh private-to-the-evaluation tasks help assess
that limitation without making contamination-free claims.

## Admitting a case

1. Freeze the task-time requirements, repository instructions, start revision,
   and relevant historical review findings. A current PR body may describe later
   repairs and must not automatically become the candidate prompt.
2. Reproduce the starting problem in a real isolated environment. Verify that
   the target is still unsolved at that revision. Starting at a PR's first listed
   commit is only a candidate choice until this check passes.
3. Calibrate acceptance on the starting and a known-good revision. A merged PR
   is reference material, not proof of correctness or a required patch shape.
   For unresolved PRs, record human-reviewed acceptance and leave uncertain
   dimensions unscored. Existing tests that simulate Agent output do not provide
   this suite's execution evidence.
4. Confirm actual Cursor execution, independent review, and reproducible setup.
   Windows, PostgreSQL, Vault, and GitHub requirements must be met where needed;
   missing infrastructure is an explicit incomplete result.

Keep reference patches, later PR comments, expected findings, and grader assets
outside the candidate workspace and memory. Pass task facts rather than this
whole catalog to the candidate. Historical GitHub access can reveal solutions;
use dedicated evaluation repositories with the frozen task inputs when measuring
clean replay, and record any unavoidable exposure.

## Run protocol

- Route work through Harness, not directly through a standalone Cursor command.
  Reuse the runtime submission path (`POST /api/workflows/runtime/submissions`)
  when preparing the isolated runner. Do not change the production intake batch
  or write evaluation PRs into the original source repositories.
- Use real repository copies, services, databases, and Cursor processes. Real
  restart and network operations are acceptable; fabricated Agent replies,
  fabricated CI results, and synthetic review verdicts are not.
- Let each review happen independently from the authoring session. Subsequent
  repair receives the actual review findings. Do not force an arbitrary number
  of review rounds or manufacture a rejection to make a loop occur. Report which
  paths were actually exercised; a first-pass approval does not test repair.
- Record Harness revision plus working-tree diff, prompt content/digest, Cursor
  CLI version, requested model, workflow config, environment, and source revision.
  `auto` does not identify a fixed underlying model. Record that uncertainty.
- Preserve real per-stage transcripts, workflow/job IDs, CLI session provenance,
  tool output, patch, final commit, independent review, and external check results.
  Human intervention is recorded and excluded from autonomous-success counts.
- Compare baseline and candidate on identical task starts and comparable resource
  budgets. Alternate execution order. Change one main factor at a time, such as
  handoff content. Use separate memory stores for variants and trials; never let
  a previous solution leak into the next run. Deliberate warm-memory experiments
  must use identical allowed prior history and be reported separately.
- Repeat selected cases with genuine fresh executions to assess stability. Report
  all attempts, not only the best. Never replace the baseline with a dry run or
  historical task status. Use the [live-run result template](run-template.md).

Acceptance belongs to the evaluator. It may independently reproduce behavior and
run checks after the candidate finishes, but this is not a production
`validation_commands` requirement or a new instruction controlling Agent methods.
Human/model review must remain distinguishable from deterministic observations;
the workflow's own reviewer cannot be the sole final judge of its own success.

## Results and comparison

For each attempt record: task and variant, start/final revisions, real session
and workflow IDs, elapsed time, autonomous or assisted execution, independent
correctness verdict with evidence, review findings addressed/missed, handoff
omissions, workflow stop reason, and actual cost when available. Missing Cursor
usage is `unknown`, not zero or a duration-based estimate.

Report counts and denominators separately:

- **Resolved tasks:** independently accepted results / evaluable first attempts.
- **False completion:** rejected results / runs Harness declared successful.
- **Unnecessary stop:** independently confirmed avoidable stops, with case reasons.
- **Review quality:** missed valid findings and false findings, adjudicated by
  reviewers outside the tested loop. Clean PRs are needed to measure over-reporting.
- **Handoff quality:** specific lost requirements, repeated failed approaches, and
  stale-revision decisions evidenced in the real transcripts.
- **Efficiency:** elapsed time and available usage for equivalent-quality results.
- **Coverage:** cases executed, incomplete environments, abstentions, and paths
  actually reached. Also report accepted tasks / all scheduled tasks so excluding
  infrastructure failures cannot make a broken system look effective.

For paired variants list improved, regressed, unchanged, and inconclusive cases.
With 15 tasks, one case changes the observed rate by about 6.7 percentage points;
do not infer a reliable improvement from one favorable run. Report uncertainty
and inspect discordant cases before promoting a change.

The existing CLI's `--k` option does not execute k trials. Do not present its
statistical estimate as observed repeated-run reliability. Report actual repeated
attempts separately, and avoid a combined score that conceals false completion.

## Real concurrency batch

After individual cases are calibrated, compare the same batch under low
concurrency and the intended 20-worker setting, using actual Cursor work. If
fewer than 20 eligible cases exist, report the exercised peak rather than claiming
20-way coverage. Include actual periodic review alongside Issue/PR work to observe
fairness. Compare task throughput, queue wait, real process activity, duplicate
submissions, workspace interference, terminal outcomes, and resource contention.
Fresh independent copies are required for repeated tasks.

## Existing implementation boundaries

The current eval CLI implements `--execute` and report comparison; the July
analysis in `docs/references/eval-flywheel.md` is historical and must not be used
as current proof that live execution is absent.

However, `eval/manifest.rs` binds cases to an isolated `remote_host`, and the
manifest requires verification commands or a registered trusted verifier. The
current production quality gate also requires server validation commands. Those
contracts have not been changed by this catalog. Verify Cursor availability on
the isolated host and separate evaluator grading from the production workflow
before declaring this suite executable. Do not add per-repository hardcoded
commands or disable checks merely to make this catalog load.

## Reuse decision

Reuse existing case provenance and runtime evidence/reporting where their
semantics match. Adapt the execution boundary only after a real pilot exposes the
minimum required integration. Do not build another scheduler or memory platform.

- [SWE-Bench Pro](https://labs.scale.com/papers/swe_bench_pro): candidate source for
  complex Issue work; select relevant public tasks and preserve dataset terms.
- [Terminal-Bench 2.1](https://www.tbench.ai/news/terminal-bench-2-1): candidate source
  for real terminal work and environment calibration. Its 28 corrected tasks
  illustrate why task validity must be checked separately from Agent quality.
- [SWE-PRBench](https://arxiv.org/abs/2603.26130): review-task and context-comparison
  methodology. Its context findings motivate experiments, not a universal rule
  that shorter prompts win. Static model review alone cannot establish our live
  repair-loop reliability.
- [Harbor](https://www.harborframework.com/docs/agents): assess reusable environment
  and trajectory formats. Using Harbor to call Cursor directly would bypass the
  Harness under evaluation; full-system integration is not yet verified.

Public benchmark imports, a held-out cohort, calibrated graders, and a live
baseline remain explicit next steps. No benchmark score is claimed by this change.
