# Harness / Cursor quota incident — operator and engineering handoff

Snapshot: **2026-09-13 20:07 Asia/Shanghai**. Refresh live facts before acting.

Repository: `/Users/apple/Desktop/code/AI/tool/harness`.
Branch: `main`; HEAD: `8142db976583b5eb229807404a1942230513013c`.

## Executive summary

The Mac's long-running Harness production batch uses the authenticated Cursor
CLI account without requiring an open Cursor editor. The production runtime
database contains 3,578 Cursor activity records since September 8. On September
13 at 16:52:50, Cursor explicitly rejected execution because the monthly usage
limit had been reached. Harness recognized the quota error but treated recovery
as a timed circuit-breaker probe, repeatedly retrying despite the reported
September 16 billing-cycle reset.

The recent 15-case development evaluation was real and completed, but it was a
separate, much smaller consumer of the same provider account. Its server on
19840 was stopped. Production on 19800 and two older evaluation services on
19810/19820 are still listening. Prior assistant reporting inspected only the
recent evaluation and failed to make that limited scope clear.

This handoff records findings and proposed work. **It does not claim that the
quota-handling defects have been fixed, that the remaining services have been
stopped, or that all production PRs can merge.**

## User intent and execution boundaries

- The user authorized autonomous Issue/PR handling through Harness using real
  Cursor, including at least 20 parallel workers in the earlier production work.
- The user explicitly removed the three-round repair restriction. Do not restore
  that arbitrary restriction as a quota fix. Business repair autonomy and
  provider-availability controls are separate concerns.
- Business repository fixes belong to Cursor inside Harness. The supervising
  assistant may inspect results, operate authorized runtime recovery, and repair
  Harness itself; it must not take over implementation of business Issues/PRs.
- Real evaluation is required. No fabricated Agent responses, impersonated
  provider processes, SQL-forced outcomes, fake review verdicts or invented CI.
- The latest request is to produce this handoff. No stop, restart, new batch,
  billing change, model change, merge or production deployment was performed
  while preparing it. Preserve earlier authorization, but do not infer a new
  operational action from this document alone.
- No new spending limit or paid overage was enabled. Do not do so implicitly.
- Keep conversation in Chinese and repository artifacts in English.

## Live service inventory

These PIDs and states were checked locally at the snapshot time. They are not
permanent identifiers or instructions to kill a process without rechecking it.

| Port | PID | Binary | Observation |
|---|---:|---|---|
| 19800 | 78607 | `cursor-batches/20260908-233757/binary/harness-no-round-cap` | Production service listening; current process has run about 41 hours |
| 19810 | 49979 | `cursor-batches/20260908-233757/binary/harness-local-review-ready` | Older evaluation service listening |
| 19820 | 59638 | `cursor-evals/20260910-170001-native-fixed/binary/harness-autonomous-intake` | Older evaluation service listening |
| 19840 | — | `cursor-evals/20260912-full-native/binary/harness-plan-contract-fix` | Recent evaluation server stopped after completion |

Relative binary paths above start at `/Users/apple/.local/share/harness/`.
At the last process-tree inspection, the three listening services had **no
descendant processes**. Production had no running runtime jobs and nine pending
jobs. A live server, an active workflow stage, and a running Cursor process are
different facts.

Production root:
`/Users/apple/.local/share/harness/cursor-batches/20260908-233757`.

Production configuration files include `cursor.toml` and `cursor-auto.toml`.
The inspected configuration specifies default agent `cursor` and
`max_concurrent_tasks = 20`. Do not print complete configuration files: they
contain credentials. Confirm the live launch configuration before changing it.

PostgreSQL container: `harness-cursor-batch-20260908-233757`.
Production database: `cursor_batch`; SQL user: `harness`.
Relevant schemas: `workflow_runtime`, `task_db`.

## Verified production measurements

The production database's first recorded runtime job was created at September
8, 23:44:47 Asia/Shanghai. The last creation timestamp in this snapshot was
September 13, 16:55:36. The batch directory and launch history begin earlier;
do not confuse directory creation, a binary replacement, process uptime and
the first persisted activity.

All 3,578 records are labeled runtime kind `cursor`, profile `cursor-default`.
This is the full database history, not 3,578 completed Issues or proof that every
record incurred a successful model request.

| Runtime job status | Count |
|---|---:|
| succeeded | 2,957 |
| failed | 475 |
| cancelled | 137 |
| pending | 9 |
| running | 0 |

| Activity | Records, including failures/cancellations/pending |
|---|---:|
| run_local_review | 1,640 |
| address_pr_feedback | 742 |
| plan_issue | 495 |
| implement_issue | 279 |
| implement_prompt | 190 |
| merge_pr | 94 |
| start_child_workflow | 69 |
| run_quality_gate | 43 |
| inspect_pr_feedback | 26 |

Review and feedback repair represent about two-thirds of recorded activities.
This supports substantial review/repair execution, not a conclusion that all
cycles were useful. Activity count is not a cost-weighted metric. The 94 merge
activity records do not establish 94 successful merges.

Workflow states at inspection:

| State | Count |
|---|---:|
| done | 260 |
| blocked | 145 |
| ready_to_merge | 95 |
| cancelled | 48 |
| failed | 27 |
| no_actionable_feedback | 13 |
| feedback_found | 13 |
| local_review_gate | 9 |
| awaiting_dependencies | 6 |

The legacy `task_db.tasks` status aggregation returned no rows. The displayed
task counts must therefore not be assumed to describe legacy TaskStore rows.
The user's pasted Dashboard snapshot reported 260 done, 17 failed, 101 queued,
9 running and 104 project copies. Its exact filters and projection were not
reproduced in this inspection. Do not combine those numbers with raw workflow
or runtime-job counts as if they were one population.

## Quota evidence and timeline

Primary production log:
`/Users/apple/.local/share/harness/cursor-batches/20260908-233757/data/logs/harness-serve-20260911T190822Z-pid78607.log`.

At **2026-09-13 16:52:50.073 +08:00**, the log records Cursor's terminal failure
with `ActionRequiredError` and the following provider message:

```text
You've hit your usage limit
You've saved $3502 on API model usage this month with Ultra.
Switch to a different model or set a Spend Limit to continue with Auto.
Your usage limits will reset when your monthly cycle ends on 9/16/2026.
```

The provider message is evidence of the usage limit and reported reset date.
The `$3502` wording is an API-price savings display, not proof of an additional
$3,502 charge, an exact invoice, or a known token total. No billing dashboard or
invoice export was obtained in this incident investigation. The login email is
intentionally omitted from this repository document.

The inspected log contains **48 quota-error lines identifying 24 distinct
runtime job IDs**. The same failure is logged by multiple layers. Do not count
48 lines as 48 independent requests.

Observed circuit openings were approximately 16:53, 17:03, 17:23, 18:03 and
19:23. The final recorded cooldown ends at **21:23:39 Asia/Shanghai on September
13**. These are historical snapshot facts; inspect current state before relying
on the next-probe time.

The log also contains `runtime pool starved` before quota exhaustion: its first
occurrence in this process log is September 12 at 03:10. At September 13, 20:00,
it reports `empty_ticks=5300`, `gated_workflows=151`, `pending_commands=0`.
There were 2,618 matching log lines in the inspected file. Tick count, log-line
count and model-call count are different. The 151 gated workflows cannot all
be attributed to quota without examining their individual stop reasons.

## Cause analysis and confidence

### 1. Authorized background execution consumed a shared provider account

Verified: Harness invokes Cursor CLI with `--print`, `--force`, `--trust` and
stream JSON. An open editor window is unnecessary. Current source rejects a
requested USD budget because the Cursor integration cannot enforce that budget.
See [Cursor adapter](../crates/harness-agents/src/cursor.rs).

Strong inference: production is the main contributor relative to the recent
103-activity evaluation, given its much larger recorded workload and direct
quota failures. Unknown: the exact dollar/token share of each service and other
account usage. Do not claim the old evaluation services were continuously
generating merely because they remained listening.

### 2. Monthly exhaustion follows transient-error recovery semantics

Verified by logs: `quota-interactive-wait` opens a breaker and later probes.
Current source implements increasing cooldowns and a single half-open probe;
failure reopens the breaker. It groups quota waiting with failures that can trip
the generic profile breaker.

This control reduces the failure burst; it does not persistently pause work
until the reported billing reset or an operator/provider availability change.
Monthly exhaustion should not be handled identically to a transient rate limit.

Source references:
[breaker](../crates/harness-server/src/runtime_circuit_breaker/mod.rs),
[classification](../crates/harness-server/src/runtime_circuit_breaker/failure_class.rs),
[policy](../crates/harness-core/src/config/workflow_circuit_breaker.rs).

### 3. Availability control is process-local, not account-wide

Current source stores breakers in an in-memory map keyed by runtime profile;
server initialization constructs a fresh registry. It does not provide shared
account-level exclusion across the three services. Restart can lose this
in-memory breaker state; persisted job deferrals may still delay individual
jobs, so do not claim restart necessarily runs every job immediately.

See [initialization](../crates/harness-server/src/http/init.rs) and the breaker
implementation. These source observations match the recorded behavior but the
running production binary is older than the dirty checkout. Verify its exact
build provenance before claiming a line-for-line production diagnosis.

### 4. Execution, outcome and availability are not clearly separated in the UI

Verified: the pasted Dashboard's nine running items coexist with nine pending
runtime jobs and no descendant Agent processes in this inspection. Current
source has workflow-to-task projections, but the precise old Dashboard path
has not been traced. Do not assert the exact rendering bug without that trace.

The operator needs to distinguish workflow stage, job lease/execution, provider
availability and merge readiness. `done` or successful review execution alone
does not prove the PR's requested behavior or required CI has passed.

### 5. Missing usage telemetry was represented as zero

The Cursor adapter writes default token usage. All 103 recent evaluation
transcripts contain zero input/output/total tokens and zero USD. These are
unavailable measurements, not evidence of zero consumption. A dashboard should
not present them as measured zero, and a budget control cannot enforce an
accurate USD ceiling using these fields.

### 6. Operator reporting and service cleanup were incomplete

The assistant previously inspected only port 19840, found no quota error there,
and stopped that evaluation service. It did not inspect the still-running
production batch before answering the account-wide quota question. Saying
“the evaluation service is stopped” without its port and scope was misleading.

Future handoffs must identify every task-owned service left running, its owner,
purpose and stop condition, rather than treating completion of one evaluation
as completion of all background work.

## Recent evaluation: preserve the valid evidence and its limits

Full report: [15-case native development run](../evals/cursor-real/full-native-20260912.md).

- Window: September 12, 20:21 through September 13, 01:55 Asia/Shanghai.
- Isolated server 19840; database `cursor_full_native_20260912`; up to four real
  Cursor processes; requested model `auto`.
- 103 activities: 97 successful completions, six failures. There were five
  planning-result contract failures and one silent-stream timeout while waiting
  for Windows CI, not quota failures in this run.
- All 15 native workflows reached `ready_to_merge`. Latest independent grader
  labels: 14 ACCEPTED, one INCOMPLETE. This is not a strict autonomous 14/15
  acceptance score: grader caveats and supervisor interventions remain material.
- Keepline's genuine provider ownership recovery remains incomplete. Historical
  candidate versions without Cursor used genuine supported providers for some
  nested checks; no nested Cursor integration is claimed for those versions.
- Bun's real Web Build and Rust Test passed; overall CI remained red on Clippy
  and Security Audit. Its temporary evaluation-repo Actions permission change
  was restored to read. No evaluation PR was merged.
- One evaluation WORKFLOW was replaced by repository defaults, introducing a
  Codex-only option into Cursor dispatch and delaying unrelated commands.
  Restoring the archived operator WORKFLOW resumed dispatch. This workaround
  did not fix config isolation or dispatcher failure containment in the product.
- The run changed prompt/configuration during execution and used development
  cases. It is not a fixed-version, calibrated, held-out comparison.

The earlier token estimate applies only to these 103 outer transcripts:

| Visible payload, approximate o200k tokenization | Tokens |
|---|---:|
| Initial prompts | 593,085 |
| Tool results | 5,032,503 |
| Visible assistant text | 274,280 |
| Tool arguments, including commands/code | 869,294 |
| Total | 6,769,162 |

Repeated-context scenarios suggested roughly 110–270 million cumulative input
tokens under assumed context caps and tool batching. This is **not measured API
usage, a confidence interval, or billed tokens**. Unknowns include internal
compression, parallel calls, hidden instructions/reasoning, the actual model
tokenizer, cache hits, provider tool serialization and nested Agent execution.
Do not extrapolate these estimates to the production batch or price all repeated
input as uncached input.

## Repository and deployment state

The checkout is substantially dirty from multiple concurrent workstreams. Do
not stage all files, reset the tree, overwrite a refactor, or rebuild production
from the current tree without isolating the intended change and reading current
repository instructions.

Prior integration status is documented in
[prompt integration](prompt-workflow-integration-20260912.md) and
[integration evidence](../evals/cursor-real/prompt-integrated-replay-20260912.md).
Those changes were not committed, pushed or deployed to production by the recent
evaluation work. A production binary name does not prove those changes are live.

Changes attributable to the full evaluation turn, captured by Patch Guard:

- Planning prompt clarification in
  `crates/harness-server/src/workflow_runtime_worker/prompt_packet/mod.rs`.
- `evals/cursor-real/README.md` and `evals/cursor-real/full-native-20260912.md`.

The prompt clarification keeps true blockers blocking and requires `[]` when
implementation can proceed. It does not weaken status validation. The isolated
candidate passed 57 focused prompt-packet tests and a CLI build, and all five
real replans proceeded. This handoff adds documentation only; no new runtime
fix is implied.

## Proposed next work, in order

### P0 — Contain continued consumption when operation is authorized

1. Refresh listener/process ownership, active jobs, breaker state and queued
   work for all three remaining services.
2. Stop or pause the intended production/evaluation services gracefully. If
   jobs are active, preserve their evidence and use the established drain path
   where possible. Do not delete queues or mark jobs successful to stop work.
3. Verify listener closure and descendants. Report exact ports stopped and any
   services intentionally left running. Verify that a supervisor does not
   immediately restart them.
4. Preserve databases, candidate worktrees, job/event/transcript history and
   current error evidence. Do not wipe pending work, reset failed history, or
   enable paid overage as a recovery shortcut.

### P1 — Correct quota waiting with the smallest product change

- Separate explicit monthly/account exhaustion from short-lived throttling and
  authentication failures. Do not build a broad speculative error taxonomy.
- Use an existing durable runtime control path where practical to suspend new
  work for the affected provider/profile. Preserve the provider's stated reset
  information; if it gives only a date, do not invent an exact reset time.
- Ensure the pause survives a server restart. A routine retry must not consume
  more workflow repair attempts or reinterpret quota as a code defect.
- At a known recovery boundary or explicit resume, allow a bounded genuine
  availability check before releasing the queue. Do not fan out 20 probes.
- Decide how existing co-running services share the pause. Prefer a single
  clearly owned production service plus stopped idle eval services initially;
  do not introduce a distributed quota platform without a concrete requirement.

### P1 — Make state and cost reporting truthful

- Show “paused: Cursor usage limit” and its recovery condition prominently.
- Distinguish actual running jobs from workflows waiting at a review stage.
- Show missing usage as unknown/unavailable, not zero. Keep local estimates
  separate from provider-reported usage and billing.
- Preserve independent review and fresh required CI as separate merge facts.

### P2 — Audit useful progress and configuration containment

- Audit repeated reviews of an unchanged SHA, recurring identical findings,
  repair-to-review evidence handoff, and infrastructure failures misrouted to
  code repair. Report per-workflow evidence before changing prompts.
- Separate new findings from duplicate findings and meaningful repairs from
  retries. The review/repair activity majority alone does not prove waste.
- Fix operator WORKFLOW isolation and per-command dispatcher error containment
  exposed by the full evaluation; include the underlying error cause in logs.
- Resolve individual gated-workflow reasons. Do not bulk-unblock all 151 items
  merely because account availability later recovers.

## Verification and acceptance for a follow-up implementation

Use repository-provided focused checks; do not run `cargo test --workspace`.
PostgreSQL tests must use a narrow test filter and disposable isolated database.
Changes to a spawn path require the repository's harness-agents tests. Read
current AGENTS.md before editing Rust, committing or pushing.

Real operational acceptance should demonstrate:

1. A genuine provider quota refusal produces a visible durable pause without
   rewriting the task as completed or generating a repair prompt for code.
2. No repeated automatic Cursor spawning occurs while the pause applies.
3. Server restart preserves the pause and does not duplicate queued work.
4. Any other service sharing the affected execution scope obeys the intended
   pause, or is explicitly stopped; the UI identifies this scope.
5. After real availability recovery or an explicitly authorized resume, a
   bounded genuine check succeeds before backlog execution continues.
6. A normal transient failure retains appropriate recovery, and code-review
   repair remains autonomous without a three-round cap.

Do not fabricate provider failures or replies to claim this acceptance. Existing
real incident logs can support classification analysis, but are not a fresh
end-to-end validation of a new implementation. Record blocked verification
honestly if account availability prevents a genuine run.

For merge, preserve the repository rule: fresh independent review, passing
`CI Result` and squash merge. No evaluation verdict grants blanket merge access
to the production backlog.

## Read-only refresh commands

Run these from the Harness checkout. They do not require printing config files,
tokens or process environments.

```sh
lsof -nP -iTCP:19800 -iTCP:19810 -iTCP:19820 -iTCP:19840 -sTCP:LISTEN
git status --short
git rev-parse HEAD
```

Use fresh PIDs from the listener query to inspect executable identity and
elapsed time. Do not rely on stale PIDs in this document.

```sh
docker exec harness-cursor-batch-20260908-233757 \
  psql -U harness -d cursor_batch -Atc \
  "SELECT status, count(*) FROM workflow_runtime.runtime_jobs GROUP BY status;"

docker exec harness-cursor-batch-20260908-233757 \
  psql -U harness -d cursor_batch -Atc \
  "SELECT data->>'state', count(*) FROM workflow_runtime.workflow_instances GROUP BY 1;"

docker exec harness-cursor-batch-20260908-233757 \
  psql -U harness -d cursor_batch -Atc \
  "SELECT data->'input'->>'activity', count(*) FROM workflow_runtime.runtime_jobs GROUP BY 1 ORDER BY 2 DESC;"
```

API recovery routes already exist:
`POST /api/workflows/runtime/retry` and `/api/workflows/runtime/unblock` accept
`workflow_id`, required `reason`, optional `target_state` and `evidence`.
These are mutations, **not instructions to call them during read-only refresh**.
Use the appropriate route only after inspecting the workflow's real stop reason
and ensuring provider availability. Do not force recovery with SQL.

## Evidence index

| Evidence | Location |
|---|---|
| Production recovery scope, 87 repos / 20 slots at September 10 | [production recovery report](../evals/cursor-real/production-recovery-20260910.md) |
| Production batch and logs | `/Users/apple/.local/share/harness/cursor-batches/20260908-233757/` |
| Recent full evaluation evidence root | `/Users/apple/.local/share/harness/cursor-evals/20260912-full-native/evidence/` |
| Full per-case SHAs, grader IDs and results | `final-results.json` under the evaluation evidence root |
| Runtime job, command, event and transcript exports | `runtime_jobs.json`, `workflow_commands.json`, `runtime_events.json`, `workflow_artifacts.json` under the evaluation evidence root |
| Evaluation shutdown record | `completed-server-stop.json` under the evaluation evidence root |
| Token estimate and assumptions | `token-estimate.json` under the evaluation evidence root |
| Evaluation task attribution | `patch-guard-final.json` under the evaluation evidence root |
| Built isolated source snapshot | `build-source-plan-fix/` under the evaluation evidence root |
| Source integration and earlier pilots | [integration note](prompt-workflow-integration-20260912.md), [PR requirements](../evals/cursor-real/pr-requirements-20260912.md), [queued restart](../evals/cursor-real/cursor-queued-restart-20260912.md) |

Raw transcripts/configuration may contain private repository content and secrets.
Keep them local; do not attach raw exports or config files to a public Issue/PR.

## Remaining unknowns and completion boundary

- Exact provider-billed tokens, cache usage, costs and per-service attribution.
- Whether all production repair/review cycles made useful progress.
- Exact production binary-to-source mapping and the old Dashboard projection.
- Individual causes of the blocked/gated workflows and the true merge readiness
  of the production PR backlog.
- Whether other independent clients consumed the same account during the cycle.

This handoff is complete as documentation. The incident remains operationally
unresolved: **19800, 19810 and 19820 were still listening at the final snapshot;
no quota-pause product fix or production rollout has been performed.**
