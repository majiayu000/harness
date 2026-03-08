# Harness — Issues Encountered During Gap Closure

> Date: 2026-03-03
> Scope: Issues found and fixed during the 13-issue gap closure sprint (PR #23 — #31)
> Context: Protocol coverage expansion from 50% to ~72%, adding persistence, review loop automation, and self-orchestration

---

## Issue 1: Review Loop Merges Unreviewed Code (Critical)

**PRs**: #28 (partial fix), #31 (complete fix)

### Symptom

PR #26 timeline shows the problem clearly:

```
06:03:27  Commit 1: feat (initial implementation)
06:04:12  Gemini: Summary comment posted
06:06:07  Gemini: Code review — 1 high + 2 medium severity issues
06:10:47  Commit 2: fix (addressed all review comments)
06:15:04  PR merged into main
          ↑ NO second Gemini review ever occurred
```

The fix commit (e618341) was merged without any verification. Gemini never reviewed the new code.

### Root Cause — Two Layers

**Layer 1 (Prompt defect)**: `review_prompt()` in `prompts.rs` had an `is_last_round` conditional:

```rust
// OLD CODE — prompts.rs:55-62
let push_action = if is_last_round {
    format!("commit, push, then run `gh pr comment {pr} --body '/gemini review'`...")
} else {
    // This line was the bug:
    "commit and push (do NOT trigger /gemini review this round to avoid feedback inflation)".into()
};
```

For a task with `max_rounds=5`, round 2's `is_last_round=false`, so the prompt explicitly told the agent **not** to trigger `/gemini review` after pushing fixes. The "avoid feedback inflation" reasoning was wrong — you cannot skip verification of new code.

**Layer 2 (Timing defect)**: Even after PR #28 removed the `is_last_round` conditional (making every round trigger `/gemini review`), the problem persisted because of an **async timing race**:

1. **Round N**: Agent fixes issues → pushes commit → triggers `/gemini review` → outputs `FIXED`
2. **Task runner**: Waits `wait_secs` (default 120s)
3. **Round N+1**: Agent runs `gh api repos/{owner}/{repo}/pulls/{pr}/reviews` → **Only the OLD review is visible** (Gemini hasn't completed re-review within 120s) → Old comments are all addressed → No unresolved comments → Agent outputs `LGTM`
4. **Task runner**: Sees LGTM → Marks task done → Queue script merges PR

The agent has no mechanism to distinguish "all comments resolved, code is verified" from "all comments resolved, but new code was never reviewed."

### Fix — Two-Layer Defense (PR #31)

**Prompt layer** — Added `prev_fixed: bool` parameter to `review_prompt()`:

```rust
pub fn review_prompt(issue: Option<u64>, pr: u64, round: u32, prev_fixed: bool) -> String {
```

When `prev_fixed=true`, the prompt prepends a **freshness check** requiring the agent to:
1. Get the timestamp of the most recent Gemini review via `gh api .../reviews --jq '.[-1].submitted_at'`
2. Get the timestamp of the latest commit via `gh api .../commits --jq '.[-1].commit.committer.date'`
3. If latest review timestamp < latest commit timestamp → Gemini hasn't re-reviewed yet → Output `WAITING`
4. Only proceed with normal LGTM/FIXED evaluation if review covers latest commit

**Task runner layer** — Added `WAITING` state handling in the review loop:

```rust
// task_runner.rs — review loop is now while-loop with prev_fixed tracking
let mut prev_fixed = false;
let mut round = 2u32;
let max_waiting_retries = 3u32;

while round <= last_review_round {
    // ... execute agent with review_prompt(issue, pr_num, round, prev_fixed) ...

    if prompts::is_waiting(&resp.output) {
        // Don't consume a round — retry with same round number
        // Up to max_waiting_retries to avoid infinite loop
        continue;
    }

    if prompts::is_lgtm(&resp.output) {
        // Safe to merge — review covers latest commit
        return Ok(());
    }

    prev_fixed = true; // Agent pushed code, next round needs freshness check
    round += 1;
}
```

**Output parser** — Added `is_waiting()` alongside `is_lgtm()`:

```rust
pub fn is_waiting(output: &str) -> bool {
    last_non_empty_line(output) == Some("WAITING")
}

// Extracted shared helper
fn last_non_empty_line(output: &str) -> Option<&str> {
    output.lines().rev().find(|l| !l.trim().is_empty()).map(|l| l.trim())
}
```

### Key Lesson

Prompt-only fixes are fragile for async workflows involving external services. The orchestrator must **structurally** enforce timing constraints. The three-state output model (LGTM / FIXED / WAITING) makes the review loop a proper state machine instead of relying on implicit assumptions about external service timing.

### Files Changed

| File | Change |
|------|--------|
| `crates/harness-core/src/prompts.rs` | Added `prev_fixed` param, freshness check block, `is_waiting()`, extracted `last_non_empty_line()` |
| `crates/harness-server/src/task_runner.rs` | `for` loop → `while` loop with `prev_fixed` tracking and WAITING retry |
| `crates/harness-cli/src/cmd/pr.rs` | Same `prev_fixed` + WAITING handling for CLI review command |

---

## Issue 2: spawn_task Signature Mismatch After EventStore Addition

**PR**: #27

### Symptom

`cargo test` fails with:
```
error[E0061]: this function takes 5 arguments but 4 arguments were supplied
   --> crates/harness-server/src/task_runner.rs:471:9
```

### Root Cause

Added `events: Arc<harness_observe::EventStore>` as 4th parameter to `spawn_task()` for PR review event logging. Updated `http.rs` (`create_task` handler) and `router.rs` (`GcAdopt` handler) call sites — but forgot the test `skills_are_injected_into_agent_context`.

`cargo check` passed because it only checks non-test code. The error only surfaces with `cargo test` which compiles `#[cfg(test)]` modules.

### Fix

```rust
// Before:
spawn_task(store, agent_clone, skills, req).await;

// After:
let events = Arc::new(harness_observe::EventStore::new(dir.path())?);
spawn_task(store, agent_clone, skills, events, req).await;
```

Also changed test signature from `async fn skills_are_injected_into_agent_context()` to `async fn skills_are_injected_into_agent_context() -> anyhow::Result<()>` for consistent `?` error propagation.

### Key Lesson

`cargo check` does NOT compile test code. When changing any public function signature, **always run `cargo test`** to catch test compilation failures. Add this to the verification rhythm: `cargo check` after every change, `cargo test` before commit.

---

## Issue 3: No Serial Task Execution — Parallel Agents Cause Merge Conflicts

### Symptom

Submitting issues #20, #21, #22 simultaneously to `POST /tasks` causes agents to run in parallel. All three modify the same files (`router.rs`, `http.rs`), producing git merge conflicts when creating PRs.

### Root Cause

`POST /tasks` immediately calls `tokio::spawn(run_task(...))`. There is no queue, no dependency mechanism, no locking. Each task gets its own worktree branch, but merging back to main is sequential — the second PR conflicts with the first.

### Workaround

External bash script for serial execution:

```bash
#!/bin/bash
for issue in 20 21 22; do
    # Submit
    TASK_ID=$(curl -s -X POST http://localhost:9800/tasks \
        -H 'Content-Type: application/json' \
        -d "{\"issue\": $issue}" | jq -r '.task_id')

    # Poll until done
    while true; do
        STATUS=$(curl -s http://localhost:9800/tasks/$TASK_ID | jq -r '.status')
        [ "$STATUS" = "done" ] || [ "$STATUS" = "failed" ] && break
        sleep 30
    done

    # Merge
    PR_URL=$(curl -s http://localhost:9800/tasks/$TASK_ID | jq -r '.pr_url')
    PR_NUM=$(echo $PR_URL | grep -oP '\d+$')
    [ -n "$PR_NUM" ] && gh pr merge $PR_NUM --squash --delete-branch
done
```

### Proper Fix (Not Yet Implemented)

Need one of:
- `POST /tasks/queue` endpoint that internally serializes dependent tasks
- Task dependency mechanism (`blockedBy` field linking tasks)
- Repository-level locking that prevents concurrent modifications

---

## Issue 4: `load_builtin()` Was Private

### Symptom

```
error[E0624]: method `load_builtin` is private
  --> crates/harness-server/src/http.rs
```

### Root Cause

`build_app_state()` in `http.rs` needs to initialize the RuleEngine with builtin rules:
```rust
let mut rule_engine = harness_rules::engine::RuleEngine::new();
rule_engine.load_builtin()?; // Error: private method
```

`load_builtin()` was originally only called from within the `harness-rules` crate itself.

### Fix

One-line change in `crates/harness-rules/src/engine.rs`:
```rust
// Before:
fn load_builtin(&mut self) -> Result<()> {
// After:
pub fn load_builtin(&mut self) -> Result<()> {
```

---

## Issue 5: stdio Transport Type Mismatch After Router Refactor

### Symptom

```
error[E0308]: mismatched types
expected `&AppState`, found `&HarnessServer`
  --> crates/harness-server/src/stdio.rs
```

### Root Cause

Router's `handle_request` was refactored from:
```rust
pub async fn handle_request(server: &HarnessServer, req: RpcRequest) -> RpcResponse
```
to:
```rust
pub async fn handle_request(state: &AppState, req: RpcRequest) -> RpcResponse
```

The HTTP transport (`http.rs`) was updated to pass `&state`, but the stdio transport (`stdio.rs`) still passed `&server`.

### Fix

Changed `serve_stdio()` to:
1. Take ownership of `HarnessServer` (not borrow)
2. Build full `AppState` via `build_app_state(Arc::new(self)).await`
3. Pass `&state` to `handle_request`

This ensures both transports (HTTP and stdio) use identical `AppState` initialization, preventing divergence.

---

## Issue 6: ThreadManager Missing 5 Lifecycle Methods

### Symptom

```
error[E0599]: no method named `find_thread_for_turn` found for struct `ThreadManager`
error[E0599]: no method named `steer_turn` found for struct `ThreadManager`
error[E0599]: no method named `resume_thread` found for struct `ThreadManager`
error[E0599]: no method named `fork_thread` found for struct `ThreadManager`
error[E0599]: no method named `compact_thread` found for struct `ThreadManager`
```

### Root Cause

The protocol (`methods.rs`) defines `TurnSteer`, `ThreadResume`, `ThreadFork`, `ThreadCompact` methods and the router handlers reference them, but `ThreadManager` only had basic CRUD operations (start_thread, get_thread, list_threads, delete_thread, start_turn, cancel_turn).

### Fix

Added 5 methods + 1 accessor to `crates/harness-server/src/thread_manager.rs`:

| Method | Purpose |
|--------|---------|
| `find_thread_for_turn(turn_id)` | Reverse lookup: find parent thread from a turn ID |
| `steer_turn(thread_id, turn_id, instruction)` | Inject mid-execution instruction into a running turn |
| `resume_thread(thread_id)` | Resume a paused thread (set status back to Active) |
| `fork_thread(thread_id, from_turn)` | Branch from a specific turn, creating new thread with history up to that point |
| `compact_thread(thread_id)` | Clear completed turn details to reduce memory (keep metadata only) |
| `threads_cache()` | Expose internal DashMap for loading persisted threads at startup |

---

## Issue 7: tempfile Dev-Dependency Missing

### Symptom

```
error[E0433]: failed to resolve: use of undeclared crate or module `tempfile`
  --> crates/harness-server/src/thread_db.rs (test module)
```

### Root Cause

New `thread_db.rs` tests use `tempfile::tempdir()` for creating temporary SQLite databases, but the crate wasn't declared in `Cargo.toml`.

### Fix

Added to `crates/harness-server/Cargo.toml`:
```toml
[dev-dependencies]
tempfile = "3"
```

---

## Issue 8: GitHub PR URL Parsing Breaks on Real-World URLs

### Symptom

`extract_pr_number()` returns `None` for URLs like:
- `https://github.com/owner/repo/pull/42/files` (opened from GitHub "Files changed" tab)
- `https://github.com/owner/repo/pull/42#discussion_r123` (linked from review comment)

### Root Cause

Original implementation naively split on `/` and tried to parse the last segment as a number. But `/42/files` yields `"files"` as the last segment, and `/42#discussion_r123` yields `"42#discussion_r123"` which fails `parse::<u64>()`.

### Fix

```rust
pub fn extract_pr_number(url: &str) -> Option<u64> {
    let without_fragment = url.split('#').next()?;  // Strip fragment first
    let parts: Vec<&str> = without_fragment.split('/').collect();
    for (i, &part) in parts.iter().enumerate() {
        if (part == "pull" || part == "pulls") && i + 1 < parts.len() {
            if let Ok(n) = parts[i + 1].parse::<u64>() {
                return Some(n);  // Parse the segment right after "pull"
            }
        }
    }
    None
}
```

This finds the `pull` keyword, then parses the **next** segment (which is always the PR number), ignoring any trailing path components.

### Test Cases Added

```rust
extract_pr_number("https://github.com/owner/repo/pull/42/files")     // → Some(42)
extract_pr_number("https://github.com/owner/repo/pull/42/commits")   // → Some(42)
extract_pr_number("https://github.com/owner/repo/pull/42#discussion") // → Some(42)
```

---

## Issue 9: Async Task Panics Leave TaskStore in Inconsistent State

### Symptom

If an agent task panics (e.g., due to an unexpected error inside the spawned future), the TaskStore entry stays in `Implementing` or `Reviewing` status forever. No error is recorded.

### Root Cause

Original code used a single `tokio::spawn`:
```rust
tokio::spawn(async move {
    run_task(&store, &id, ...).await  // If this panics, no cleanup
});
```

When the inner future panics, tokio catches the panic as a `JoinError`, but nobody is `await`-ing the `JoinHandle`, so the error is silently lost. The TaskStore entry never transitions to `Failed`.

### Fix — Two-Spawn Watcher Pattern

```rust
// Inner spawn: runs the actual task
let handle = tokio::spawn(async move {
    run_task(&store, &id, agent.as_ref(), skills, events, &req, project).await
});

// Watcher spawn: observes the inner task's termination
tokio::spawn(async move {
    match handle.await {
        Ok(Ok(())) => {}  // Task completed successfully
        Ok(Err(e)) => {
            // Task returned an error — record it
            mutate_and_persist(&store_watcher, &id_watcher, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(e.to_string());
            }).await;
        }
        Err(join_err) => {
            // Task panicked or was cancelled — record it
            tracing::error!("task {id_watcher:?} panicked: {join_err}");
            mutate_and_persist(&store_watcher, &id_watcher, |s| {
                s.status = TaskStatus::Failed;
                s.error = Some(format!("task failed unexpectedly: {join_err}"));
            }).await;
        }
    }
});
```

The watcher spawn `await`s the inner `JoinHandle`, catching all three termination paths: success, error, and panic/cancellation. This guarantees the TaskStore always reaches a terminal state.

### Key Lesson

In async Rust, when a spawned task must update shared state on termination (including panics), a single spawn is insufficient. The two-spawn pattern provides guaranteed cleanup regardless of how the inner task terminates.

---

## Issue 10: Git Branch Name Confusion After Amend

### Symptom

```
$ git push origin fix/always-retrigger-review --force-with-lease
error: src refspec fix/always-retrigger-review does not match any
```

### Root Cause

Workflow was:
1. Created branch `fix/always-retrigger-review` → committed → pushed → merged as PR #28
2. Later, on a different local branch (`fix/rule-observability-pipeline`), used `git commit --amend` to update the commit
3. Tried to push to the old branch name, but local HEAD was on a different branch

`git branch` showed `* fix/rule-observability-pipeline`, not `fix/always-retrigger-review`.

### Fix

Created a new branch from current HEAD and pushed fresh:
```bash
git checkout -b fix/review-freshness-check
git push -u origin fix/review-freshness-check
```

### Key Lesson

1. Always run `git branch` before pushing to confirm which branch you're actually on
2. After amending a commit, the branch name doesn't change — but the commit history diverges from the remote
3. When the original remote branch has already been merged, don't force-push over it — create a new branch instead

---

## Issue 11: Review Loop Divergence — Agent Cannot Converge Under Aggressive Reviewer

**PR**: #33 (Phase 1 TurnInterceptor Framework)

### Symptom

Phase 1 task failed after 5 review rounds. Each round the agent fixed Gemini's findings, but each fix introduced new code that triggered new medium-severity findings. The loop never converged to LGTM.

```
Round 2: Gemini finds 5 issues (critical + medium) → Agent fixes all 5
Round 3: Gemini finds 3 new issues in the fix code → Agent fixes all 3
Round 4: Gemini finds 2 more medium suggestions → Agent fixes both
Round 5: Gemini still has 3 medium findings → max_rounds exhausted → FAILED
```

### Root Cause — Two Failures

**Failure 1: Agent blindly obeys severity labels.** The agent treated every Gemini comment as mandatory regardless of actual impact. Gemini labeled `std::env::var("HOME")` as `security-high` (not cross-platform), but harness only targets macOS/Linux — this is not a real issue. The agent fixed it anyway, producing new code for Gemini to review.

**Failure 2: Each fix expands the review surface.** Fixing comment A produces new code B. Gemini reviews B and finds issue C. Fixing C produces code D. This is a divergent loop when the reviewer (Gemini) is aggressive about medium-severity findings.

### Specific Non-Issues That Blocked Convergence

| Gemini Finding | Severity Label | Actual Impact |
|---|---|---|
| `gc.rs:13` — `project_root = PathBuf::from(".")` | medium | By design — GC operates on current directory |
| `handlers/mod.rs:40` — `std::env::var("HOME")` | security-high | macOS/Linux only; HOME always exists |
| `gc.rs:104` — user data in prompt | security-high | Already wrapped with `wrap_external_data()` |

### First Attempted Fix — Severity-Based Rules (Rejected)

Added a 3-tier system:

```
Round 2: fix critical/high/medium
Round 3: fix only critical/high, skip medium
Round 4+: convergence mode, skip all non-bugs
```

This was rejected because it's the same problem in reverse — blindly skipping by severity label is as wrong as blindly fixing by severity label. A "medium" finding could be a real bug; a "high" finding could be irrelevant to the project context.

### Actual Fix — Agent Triage with Reasoning

Replaced severity-based rules with agent-driven analysis. The review prompt now instructs:

```
For EACH review comment, analyze it and decide:
- FIX: real bug, security flaw, or correctness issue that could cause failures. Fix it.
- SKIP: style preference, theoretical concern without concrete impact, or not
  applicable to this project's context. Skip it.

Before fixing or skipping, briefly reason about WHY. Do not blindly obey the
reviewer's severity label.

Round 2 threshold: Be thorough — fix anything with reasonable chance of causing issues.
Round 3+ threshold: Be selective — only fix comments where you can articulate a
concrete failure scenario.
```

### Key Lesson

The root issue is not about severity labels or round thresholds — it's about **judgment authority**. The agent must evaluate each review comment against the project's actual constraints (target platforms, existing defenses, design decisions), not mechanically follow the reviewer's classification. This applies to any automated code review integration, not just Gemini.

### Files Changed

| File | Change |
|------|--------|
| `crates/harness-core/src/prompts.rs` | `review_prompt()` rewritten: `severity_guidance` → `triage_instruction` with FIX/SKIP decision framework |

### Related Issue

The background phase script (`scripts/submit-phases.sh`) switches git branches in the main worktree while running, which conflicts with manual work in the same directory. Required using `git worktree add` to isolate manual fixes. This directly motivates Phase 9 (Task Worktree Isolation).

---

## Issue 12: Sequential Tasks Without Worktree Isolation — Ghost Completions

**Phases affected**: Phase 4 (Preflight), Phase 7 (Cross-Review)

### Symptom

Phase 4 and Phase 7 tasks completed as `done` in ~4min and ~6min respectively, with `turn: 1, result: "implemented"`, but produced zero code changes. Their branches are byte-for-byte identical to the previous phase's branch:

```
feat/phase4-preflight  → same commit as feat/phase3-builtin-skills (e7838f5)
feat/phase7-cross-review → same commit as feat/phase6-health-stats (2f6ee6a)
```

No PR was created. The task runner saw no `PR_URL=` in agent output, so it treated the task as a non-PR task and marked it done.

### Root Cause — Three-Layer Failure

**Layer 1: Shared worktree contaminates agent context.**

All phases run sequentially in the same working directory. When Phase 4 starts, the filesystem still contains Phase 3's uncommitted or branch-local changes. The agent sees files from the wrong context:

```
Phase 3 creates skills/preflight.md (a markdown skill file)
Phase 4 starts → agent sees skills/preflight.md already exists
Phase 4 prompt says "Create preflight constraint generation"
Agent concludes: "preflight already implemented" → outputs completion without PR_URL
```

**Layer 2: Agent conflates similar names.**

Phase 3 created `skills/preflight.md` (a markdown instruction file for the "preflight" skill). Phase 4 needed `handlers/preflight.rs` (a Rust handler implementing preflight constraint generation — completely different). The agent saw "preflight" in the filename and assumed the work was done.

Same pattern for Phase 7: Phase 3 created `skills/cross-review.md`, but Phase 7 needed `handlers/cross_review.rs`.

**Layer 3: Task runner has no output validation.**

The task executor's completion logic:

```rust
let pr_url = prompts::parse_pr_url(&resp.output);
let pr_num = match pr_number {
    Some(n) => n,
    None => {
        // No PR URL → assume non-PR task → mark done immediately
        update_status(store, task_id, TaskStatus::Done, 1).await;
        return Ok(());
    }
};
```

When the agent outputs text without `PR_URL=`, the task is marked done unconditionally. There is no validation that the agent actually created any files, branches, or commits. A task that does nothing is indistinguishable from a task that completes without a PR.

### Evidence

```
$ git diff feat/phase3-builtin-skills...feat/phase4-preflight --stat
(empty — zero diff)

$ git diff feat/phase5-learn-feedback...feat/phase7-cross-review --stat
(shows only Phase 6 health/stats code — zero Phase 7 code)

$ curl localhost:9800/tasks/d15b745e...  # Phase 4
{"status":"done","turn":1,"pr_url":null,"rounds":[{"turn":1,"action":"implement","result":"implemented"}]}

$ curl localhost:9800/tasks/0cdcd75b...  # Phase 7
{"status":"done","turn":1,"pr_url":null,"rounds":[{"turn":1,"action":"implement","result":"implemented"}]}
```

### Key Lessons

1. **Worktree isolation is not optional for sequential tasks.** When tasks share a directory, each task inherits the previous task's dirty state. The agent cannot distinguish "this file exists because I created it" from "this file exists because a previous task created it".

2. **Name similarity is an LLM confusion vector.** `skills/preflight.md` vs `handlers/preflight.rs` — same concept name, completely different artifact. The agent matched on concept rather than verifying the specific deliverables.

3. **"No PR" should not mean "success".** The task runner assumes that if the prompt asked for a PR and the agent didn't create one, the task simply didn't need a PR. A safer design: if the prompt contains "open a PR" or "Print PR_URL=" but the agent output has no PR_URL, treat it as a failure rather than a silent success.

### Fix Required

See Issue 13 below for the concrete fix.

---

## Issue 13: Task Executor Silently Succeeds When Agent Produces No PR

**Root cause of**: Issue 12 (Phase 4 and Phase 7 ghost completions)

### Symptom

`task_executor.rs` marks a task as `Done` when the agent output contains no `PR_URL=`, regardless of whether the prompt asked for a PR. This means a task that does nothing is indistinguishable from a task that legitimately completes without a PR.

### The Problematic Code Path

```rust
// task_executor.rs — after agent turn 1 completes
let pr_num = match pr_number {
    Some(n) => n,
    None => {
        // OLD: silently mark done — even if the prompt said "open a PR"
        update_status(store, task_id, TaskStatus::Done, 1).await;
        return Ok(());
    }
};
```

### Fix

Added PR expectation check. If the original prompt contains PR-related instructions (`"open a PR"`, `"PR_URL="`, `"push"`) or the task was created from an issue, the task fails when no PR_URL is produced:

```rust
let pr_num = match pr_number {
    Some(n) => n,
    None => {
        let expects_pr = req.prompt.as_deref().map_or(false, |p| {
            p.contains("open a PR") || p.contains("PR_URL=") || p.contains("push")
        }) || req.issue.is_some();

        if expects_pr {
            return Err(anyhow::anyhow!(
                "Task prompt expected a PR but agent did not output PR_URL=<url>"
            ));
        }

        update_status(store, task_id, TaskStatus::Done, 1).await;
        return Ok(());
    }
};
```

This ensures:
- Tasks with explicit PR instructions fail visibly instead of silently succeeding
- Tasks without PR expectations (e.g. dry-run classification) still succeed normally
- The error message in `TaskState.error` makes it clear what went wrong

### Files Changed

| File | Change |
|------|--------|
| `crates/harness-server/src/task_executor.rs` | Added `expects_pr` check before marking task Done |

### Key Lesson

Default-to-success is dangerous in task orchestration. When a task prompt explicitly requests an output artifact (PR, file, report), the absence of that artifact should be treated as a failure, not a no-op success. Silent success hides bugs and makes debugging much harder — the Phase 4/7 ghost completions were only caught by manual inspection of branch diffs.
