# Stateless Repo Backlog Poll — Design Spec

Status: Draft v0 (proposal, pending review)
Author: harness contributor
Audience: harness-server reviewers + future implementer

## Normative Language

`MUST`, `MUST NOT`, `SHOULD`, `MAY` follow RFC 2119.

## 1. Problem Statement

Today the `repo_backlog` workflow is a **stateful, claude-driven** subsystem:

1. Each tracked GitHub repo gets a long-lived `workflow_runtime.workflow_instances` row with `definition_id = repo_backlog`, transitioning through `idle → scanning → planning_batch → dispatching → reconciling → idle ...`.
2. Server's `repo_backlog_poll` background tick (`crates/harness-server/src/http/background.rs:664-736`) enqueues a `poll_repo_backlog` activity for each idle/failed instance.
3. A claude subprocess executes the activity, calls `gh` via Bash, and emits an `harness-activity-result` fenced JSON block containing `IssueDiscovered` signals plus an optional `workflow_decision` artifact.
4. Reducer functions (`crates/harness-workflow/src/runtime/reducer.rs:186-345`) consume the structured result, transition state, and enqueue the next activity (`plan_repo_sprint` → `start_child_workflow`).
5. claude executes a second activity (`plan_repo_sprint`) to produce `SprintTaskSelected` signals → reducer creates `github_issue_pr` child workflow instances.

This delivers correct results when claude formats its JSON exactly per the wire-format spec, but is fragile in three observed ways:

- **Wire-format drift** (FACT, observed in this codebase 2026-05-07): claude emitted `signals: [{kind: …}]` instead of canonical `[{signal_type, signal}]`, and `command: {…}` instead of `commands: [{command_type, dedupe_key, command}]`. Both shapes deserialize "successfully" with serde defaults, leaving the reducer with empty arrays and the workflow stuck.
- **Empty-output silent succeed** (FACT, observed): `activity_result_from_turn` (`workflow_runtime_worker.rs:1764-1771`) maps `StructuredActivityResult::Missing` to `ActivityResult::succeeded(...)`, so a turn that produced no fenced block still advances the state machine. Reducer falls back to `finish_repo_backlog_scan`, returns to `idle`, the next 60-second tick re-dispatches, and claude burns tokens forever in a no-progress loop.
- **Dead-end middle states** (FACT, observed): two `repo_backlog` instances have been stuck in `planning_batch` and `dispatching` for >2 hours with no transition path back to a candidate state — the poller's eligibility check is `matches!(state, "idle" | "failed")`, so middle states are invisible to it.

Symphony (`/Users/apple/Desktop/code/AI/tool/symphony/SPEC.md`) does not have these problems by design: tracker (Linear) is the source of truth, the orchestrator does not maintain a multi-stage state machine for "what issues are open," and agents call tracker APIs directly via tools rather than emitting structured JSON for the orchestrator to parse.

## 2. Goals

- Remove the **mandatory** `repo_backlog` state machine (`idle → scanning → planning_batch → dispatching → reconciling`). There is no workflow instance per repo; there is no required `poll_repo_backlog` activity.
- Server fetches GitHub open-issue state directly via the existing `github_token` and `reqwest` infrastructure on a fixed cadence. **95% of issues never see claude in the discovery path.**
- For each open issue without an active `github_issue_pr` workflow, the server creates one directly through the workflow runtime API.
- **Preserve the AI-valuable subset**: cross-issue dependency analysis. Issues whose body or title contains explicit dependency markers (e.g. `depends on #N`, `blocks #N`, `requires #N`) are routed through an **on-demand `analyze_dependencies` activity** instead of being dispatched directly. This activity batches multiple issues per call so claude is invoked at most once per poll tick (not once per issue).
- Existing `github_issue_pr` workflow lifecycle (implement_issue, replanning, pr_open, pr_feedback sweep) is **unchanged**. Replan, PR feedback, address-feedback all keep their current activities.
- Polling is idempotent: re-running yields the same set of newly-created child workflows.
- No persistent state per repo beyond the existing `workflow_instances` table for `github_issue_pr`.

## 3. Non-Goals

- Not changing PR feedback sweep, reconciliation, runtime worker, or any non-backlog subsystem.
- Not changing the `github_issue_pr` workflow definition or any of its activities (`implement_issue`, `replan_issue`, `inspect_pr_feedback`, `address_pr_feedback` all stay).
- Not changing wire format for `activity_result` of unrelated activities.
- Not removing the *capability* of dependency-aware ordering. Removing only the *mandatory state-machine path* through `plan_repo_sprint`. Equivalent value comes back as the on-demand `analyze_dependencies` activity (§4.4).
- Not removing claude from agents-list. Claude still runs `implement_issue`, `replan_issue`, `inspect_pr_feedback`, `address_pr_feedback`, plus the new optional `analyze_dependencies`.

## 3a. Scope Clarification: What Gets Deleted vs Reshaped

This section is added because the goal "delete repo_backlog workflow" was too broad and caused review concern about losing AI-valuable dependency analysis. The deletion is **selective**:

| Capability | Today | After |
|---|---|---|
| Per-repo state machine | `repo_backlog` workflow + 5 states | **Deleted entirely.** Server polls GitHub by cron, no per-repo persistent state. |
| `poll_repo_backlog` activity | Mandatory every poll tick | **Deleted.** Server's reqwest call replaces it. |
| `plan_repo_sprint` activity (mandatory state-machine step) | Mandatory after every `IssueDiscovered` | **Deleted as state-machine step.** |
| Sprint dependency ordering (the AI-valuable behaviour inside `plan_repo_sprint`) | Run claude every cycle on every issue, even when no dependencies exist | **Reshaped to `analyze_dependencies` activity.** Runs only when issue body/title matches `dependency_keywords` regex. Batched per tick so one claude call orders many issues at once. |
| `start_child_workflow` (creating `github_issue_pr` instances) | Reducer-side step driven by claude signal | **Direct server call.** No claude involvement; just SQL insert. |
| `replan_issue` / replanning state in `github_issue_pr` | As-is | **Unchanged.** This stays inside the per-issue workflow; not in scope. |
| PR feedback sweep | As-is | **Unchanged.** |

## 4. Architecture

### 4.1 New module

`crates/harness-server/src/intake/github_backlog_poller.rs` (NEW, target ≤ 400 lines)

Responsibilities:

- One `tokio::spawn` loop per server instance.
- For every entry in `intake.github.repos`, on each tick:
  1. `GET https://api.github.com/repos/{owner}/{repo}/issues?state=open&per_page=100` (paginated as needed).
  2. Filter out `pull_request != null` (the GitHub /issues endpoint includes PRs).
  3. Apply `label` filter when configured.
  4. For each remaining issue: build the canonical workflow_id `format!("{project_id}::repo:{repo}::issue:{number}")`, query `WorkflowRuntimeStore::get_instance(workflow_id)`. Skip if an active workflow already exists.
  5. **Partition the remaining "uncovered" issues into two buckets:**
     - **Direct-dispatch bucket**: title + body contain no dependency keywords. Server calls `start_issue_workflow` for each, instances created in state `scheduled`. Runtime dispatcher picks them up FIFO ordered by `(priority_label_rank ASC, created_at ASC)` (deterministic Rust sort).
     - **Dependency-analysis bucket**: title or body matches `dependency_keywords` regex (default: `(?i)(depends on|blocks|requires|blocked by)\s*#\d+`). These are deferred — see §4.4.
  6. Existing `runtime_dispatch` background loop picks up `scheduled` workflows and runs `implement_issue` exactly as today.

### 4.2 Removed surface

- `crates/harness-server/src/workflow_runtime_repo_backlog.rs` (685 lines): drop everything except `repo_backlog_workflow_id` if anything still references that exact format. Most likely the whole file is removed.
- `crates/harness-workflow/src/runtime/repo_backlog.rs` (221 lines): delete.
- `crates/harness-workflow/src/runtime/reducer.rs`: drop `repo_backlog_poll_decision_from_activity_result`, `repo_backlog_sprint_plan_decision_from_activity_result`, `repo_backlog_child_dispatch_still_active`, plus `RepoBacklogIssueCandidate` struct and its helpers (~250 lines).
- `crates/harness-server/src/workflow_runtime_worker.rs`: drop `poll_repo_backlog` and `plan_repo_sprint` branches in `agent_summary_contract`, `activity_transition_contract`, prompt-packet generators, and validators (~120 lines).
- `crates/harness-server/src/http/background.rs`: replace `spawn_runtime_repo_backlog_poller` with `spawn_github_backlog_poller` calling the new module (~50 lines).
- Validators in `crates/harness-workflow/src/runtime/validator.rs`: drop the `repo_backlog` definition entry (~30 lines).

Net code change: roughly **+350 / −1100**, single PR.

### 4.3 API contracts

The new poller exposes one public function:

```rust
pub async fn poll_github_backlog_once(
    store: &WorkflowRuntimeStore,
    repo_config: &GitHubRepoConfig,
    project_root: &Path,
    github_token: Option<&str>,
    dispatcher: &DependencyAnalysisDispatcher,
) -> anyhow::Result<PollOutcome>;
```

`PollOutcome` records `fetched`, `skipped_pr`, `skipped_label`, `skipped_existing_workflow`, `started_direct`, `enqueued_for_dependency_analysis`, `errors`. Value type, not persisted.

The poller MUST be idempotent: calling `poll_github_backlog_once` twice in succession on the same input MUST produce `started_direct + enqueued_for_dependency_analysis == 0` on the second call, regardless of what the first call did.

### 4.4 On-demand dependency analysis

A second component exists ONLY for issues that explicitly mention dependencies. Most ticks have zero such issues and this component is dormant.

**Trigger condition (FACT-driven):** issue title OR body matches `dependency_keywords`. Default regex: `(?i)\b(depends on|blocks|requires|blocked by)\s*#\d+\b`. Configurable via `intake.github.dependency_keywords` in `~/.config/harness/config.toml`. Setting empty string disables on-demand analysis (everything goes direct-dispatch).

**Component:** `crates/harness-server/src/intake/dependency_analysis_queue.rs` (NEW, target ≤ 200 lines).

```rust
pub struct DependencyAnalysisDispatcher {
    pending: Arc<Mutex<HashMap<String /* repo */, Vec<PendingIssue>>>>,
    flush_threshold: usize,           // default 5
    flush_interval: Duration,         // default 300s
    activity_dispatcher: Arc<ActivityDispatcher>,
}

impl DependencyAnalysisDispatcher {
    pub fn enqueue(&self, repo: &str, issue: PendingIssue);
    async fn flush_repo(&self, repo: &str);  // dispatches one analyze_dependencies activity for batched issues
}
```

Flush conditions (whichever comes first):

- **Threshold flush**: pending count for one repo reaches `flush_threshold` (default 5).
- **Interval flush**: pending issues sit longer than `flush_interval` (default 300 seconds).

Each flush dispatches **one** `analyze_dependencies` activity, batching all pending issues for that repo into a single claude call. The activity result MUST emit signals `IssueOrdered { issue_number, position, depends_on: [N, …] }` for each input issue.

When the activity completes, server reads signals and creates `github_issue_pr` instances in state `scheduled` with `data.depends_on` populated. Existing runtime dispatcher's "wait for dependencies" logic in `github_issue_pr` workflow already handles `awaiting_dependencies` state — no new wiring needed.

If `analyze_dependencies` fails (timeout, invalid output, etc.): the batched issues fall back to **direct-dispatch with no dependency info**. Same outcome as if no dependencies were declared. NEVER retry indefinitely.

**Why this works:**

- 95% of issues skip claude entirely (direct-dispatch path, deterministic).
- 5% with explicit dependency markers get batched into one claude call per N minutes per repo.
- Failure of dependency analysis degrades gracefully (still dispatches, just without ordering).
- Single claude invocation per tick per repo at most.
- One activity, one schema, no multi-stage state machine.

**activity_result_schema for `analyze_dependencies`:**

Reuses the canonical `ActivityResult` shape (the one we just stabilised in §wire_format_example). Signals shape:

```json
{"signal_type": "IssueOrdered", "signal": {
    "issue_number": 42,
    "position": 1,
    "depends_on": [],
    "rationale": "P0 with no upstream blockers, ship first"
}}
```

The reducer for `analyze_dependencies` lives in `harness-server` not `harness-workflow` — it does not interact with any workflow state machine, only writes new `github_issue_pr` instances directly.

## 5. Migration

### 5.1 Existing repo_backlog instances

There are currently 8 rows in `workflow_runtime.workflow_instances` with `definition_id = repo_backlog`. After this change they become orphaned. Migration step:

```sql
DELETE FROM workflow_runtime.workflow_instances WHERE definition_id = 'repo_backlog';
DELETE FROM workflow_runtime.workflow_decisions WHERE workflow_id LIKE '%::backlog';
DELETE FROM workflow_runtime.workflow_commands WHERE workflow_id LIKE '%::backlog';
DELETE FROM workflow_runtime.workflow_events WHERE workflow_id LIKE '%::backlog';
DELETE FROM workflow_runtime.runtime_jobs WHERE data::jsonb->'input'->>'workflow_id' LIKE '%::backlog';
```

This MUST run as part of the deploy. The PR SHOULD include a migration file under the workflow runtime store's migration sequence so it runs automatically on next server start. If a fresh server start cannot guarantee migration ordering with running jobs, the SAFER path is a separate cleanup CLI subcommand (`harness migrate drop-repo-backlog`) the operator runs once.

### 5.2 In-flight `github_issue_pr` workflows

Untouched. They keep their state; the new poller only creates NEW ones for issues that don't yet have a workflow.

### 5.3 Dedupe of already-implemented issues

The new poller's "active workflow" check MUST consider `state IN ('done', 'pr_open', 'awaiting_feedback', 'addressing_feedback', 'ready_to_merge')` as "covered, do not create new workflow." It MAY treat `state IN ('failed', 'cancelled')` as "skipped, do not retry automatically" — operators who want to retry must explicitly reset state via DB or a future API.

## 6. Configuration

Existing `~/.config/harness/config.toml` plus optional new fields:

```toml
[intake.github]
enabled = true
poll_interval_secs = 60                       # already exists; reused
dependency_keywords = "(?i)\\b(depends on|blocks|requires|blocked by)\\s*#\\d+\\b"
                                              # NEW; default if absent. Empty string = disable on-demand analysis.
dependency_flush_threshold = 5                # NEW; default 5. Flush dep batch when N issues queued.
dependency_flush_interval_secs = 300          # NEW; default 300s. Flush dep batch on this timer regardless.

[[intake.github.repos]]
repo = "majiayu000/harness"
label = ""                                    # empty = no filter
project_root = "/Users/apple/Desktop/code/AI/tool/harness"
```

`WORKFLOW.md`'s `repo_backlog`, `runtime_dispatch.workflow_profiles.repo_backlog` sections become dead config — they MAY be left in place (ignored) or removed in a follow-up PR. `runtime_dispatch.workflow_profiles.analyze_dependencies` MAY be added by operators who want to override claude profile/model/timeout for the new activity (otherwise inherits the default profile).

## 7. Observability

New INFO log entry per tick:

```
github_backlog_poll: repo=majiayu000/loom fetched=8 skipped_pr=0 skipped_label=0 skipped_existing=7 started_new=1 elapsed_ms=243
```

Existing `/api/intake` channel SHOULD report `active = sum(github_issue_pr instances in non-terminal state)` instead of the legacy issue_workflow_store value. This unblocks the "homepage shows zeros" bug independently of this refactor — could ship in same PR or a follow-up.

## 8. Test Plan

Unit (poller):

- `poll_github_backlog_once_creates_new_workflow_for_uncovered_issue`
- `poll_github_backlog_once_skips_pr`
- `poll_github_backlog_once_skips_existing_active_workflow`
- `poll_github_backlog_once_skips_failed_workflow_by_default`
- `poll_github_backlog_once_idempotent_under_repeated_call`
- `poll_github_backlog_once_paginates_when_more_than_100_issues`
- `poll_github_backlog_once_routes_dependency_keyword_to_analysis_queue`
- `poll_github_backlog_once_direct_dispatches_when_no_dependency_keyword`
- `poll_github_backlog_once_priority_sort_applies_to_direct_dispatch_bucket`

Unit (dependency analysis dispatcher):

- `dispatcher_flushes_when_threshold_reached`
- `dispatcher_flushes_on_interval_timer`
- `dispatcher_falls_back_to_direct_dispatch_when_activity_fails`
- `dispatcher_creates_workflows_with_depends_on_from_signals`
- `dispatcher_does_not_create_duplicate_workflow_when_same_issue_re_enqueued`

Integration (mock GitHub via wiremock):

- Full server start, single repo with 5 open issues + 2 PRs + 1 closed → exactly 5 workflows created.
- Re-run poller → 0 new workflows created.
- Mark 1 of 5 as failed in DB → poller does not recreate.

End-to-end smoke (manual, post-deploy):

- Confirm logs show `github_backlog_poll` entries, no `repo_backlog poller tick complete` entries.
- Confirm new GitHub issue created in test repo gets a `github_issue_pr` workflow within `poll_interval_secs`.

## 9. Risks

- **R1: Implement_issue agent path depends on data fields populated by `plan_repo_sprint`.**
  Mitigation: audit `crates/harness-server/src/workflow_runtime_worker.rs` and `prompts.rs` for any read of `instance.data["depends_on"]` / `instance.data["sprint_plan"]`. If absent, fine. If present, populate them with empty defaults at issue-workflow creation.

- **R2: Loss of "sprint planning" intelligence.**
  The current claude-driven `plan_repo_sprint` ranks issues by P0/P1/P2 labels AND analyses cross-issue dependencies (`#42 depends on #41`). The stateless direct-dispatch path covers only the priority-label part deterministically.
  Mitigation: §4.4's on-demand `analyze_dependencies` activity recovers the dependency-analysis half. Issues without dependency keywords use deterministic priority sort (`p0` > `p1` > `p2` > unlabelled, then `created_at` ASC, then `issue_number` ASC). Issues with dependency keywords go through claude in batched form. End-state behaviour is equivalent to or better than today's `plan_repo_sprint`, with much less token spend.

- **R3: GitHub API rate limit.**
  100-per-page page fetch × 8 repos × 1-per-minute = 480 calls/hour. Well below 5000/hour authenticated limit. No concern at current scale.

- **R4: Backward compatibility for users running older `harness` binaries against newer DB.**
  Old binary will still try to dispatch `poll_repo_backlog` activities. If the new schema does not have `repo_backlog` workflow definition, dispatch fails loudly — acceptable, since deploy SHOULD upgrade in lock-step.

## 10. Phasing

Recommended order, three PRs:

**PR-A (this spec → reviewed, then merge as design doc only)**
- Just commits this file. No code change.

**PR-B (shadow mode, code change but no behavior change)**
- Implement `intake/github_backlog_poller.rs`.
- Run it in `tokio::spawn` alongside the existing `repo_backlog_poller`.
- The new poller logs but does NOT call `start_issue_workflow` — instead logs "would create workflow for issue:N".
- Compare logs over 1 hour: does the new poller surface the same issue set the old one is processing?
- Merge if shadow output matches expectation.

**PR-C (cutover)**
- Switch `start_issue_workflow` from log-only to actually creating workflow.
- Disable the legacy `spawn_runtime_repo_backlog_poller` and remove its dependencies.
- Run migration SQL.
- Delete dead code.

This phasing means PR-B is reversible (just remove the new spawn call) and PR-C is the irreversible step.

## 11. Open Questions

- Q1: Should `repo_backlog` workflow definition be deleted from `workflow_definitions` table, or just unused? (Recommendation: delete in PR-C migration.)
- Q2: Where should priority sort happen — in the poller, or as a separate `pending_workflow_priority` column on the `github_issue_pr` instance that workflow runtime worker reads on claim? (Recommendation: poller does sort; runtime worker is FIFO.)
- Q3: Should `poll_interval_secs` be per-repo or global? Currently global. (Recommendation: stay global until a per-repo need surfaces.)
- Q4: Once `poll_repo_backlog` and `plan_repo_sprint` activities are gone, the prompt-packet generation in `workflow_runtime_worker.rs` no longer needs `wire_format_example` for those activity types. Audit and shrink. (Recommendation: include in PR-C cleanup.)
- Q5: Should `analyze_dependencies` live as a workflow runtime activity (going through `workflow_runtime_worker` machinery) or as a direct ad-hoc claude spawn from the dispatcher? Activity gives observability + retry policy; direct spawn is simpler. (Recommendation: route through workflow runtime as a singleton workflow `dependency_analysis_session`, instance per repo, no state machine — just dispatch + record result. Reuses existing prompt packet + wire format.)
- Q6: Should `dependency_keywords` be settable per-repo (via `[[intake.github.repos]] dependency_keywords = "..."`) or only globally? (Recommendation: per-repo override, falling back to global default. Some repos use different conventions, e.g. `Closes #N` for PRs but `Refs #N` for issue cross-references.)

## 12. Acceptance Criteria

PR-C is mergeable when:

- `cargo test --workspace` passes.
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` passes.
- `cargo fmt --all -- --check` clean.
- A test-mode run (single test repo with 3 open issues) creates exactly 3 `github_issue_pr` instances in DB after one poll tick.
- Removing the new poller and re-enabling the old one does NOT cause migrations to fail (migrations are forward-only, but a downgrade should at worst lose runtime state, not corrupt DB).
- No `repo_backlog` strings remain in `workflow_runtime_worker.rs`, `reducer.rs`, `repo_backlog.rs`, `workflow_runtime_repo_backlog.rs`, `validator.rs`, `prompts.rs`, except in tests that explicitly cover the removal.
