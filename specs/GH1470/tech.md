# Tech Spec

## Linked Issue

GH-1470

## Product Spec

See `specs/GH1470/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Scheduler | `crates/harness-server/src/periodic_reviewer.rs` | `run_review_tick` computes the effective watermark, builds a prompt, and enqueues the primary review task before any commit-presence check. | This is the no-op agent spawn path. |
| Prompt | `crates/harness-core/src/prompts/review.rs` | The prompt tells the agent to run `git log --since=<watermark>` and output `REVIEW_SKIPPED` when empty. | This remains defense in depth, but should no longer be the normal idle path. |
| Watermark | `periodic_reviewer.rs` EventStore query plus `ReviewState::fallback_ts` | The scheduler merges persisted and fallback timestamps into the `since_arg` used by the prompt. | The local gate must use the same lower bound. |
| Cross-review | `periodic_reviewer.rs` poll task | Secondary and synthesis tasks are already deferred until the primary produces real review output. | Local primary skips should naturally prevent downstream review work. |
| Observability | `periodic_reviewer.rs` tracing plus EventStore | Successful parsed reviews log `periodic_review` events; skipped agent output only logs tracing today. | Local skips need an explicit observable signal. |
| Docs | `docs/usage-guide.md`, `docs/periodic-review.md` | Docs already describe a pre-agent skip path even though implementation still relies on the agent. | Docs should match the implementation after the fix. |

## Proposed Design

1. Introduce a small local commit-gate helper used by `run_review_tick` before
   building/enqueueing the primary review task.
   - Input: project root and effective `last_review_ts`.
   - Output: `Run`, `SkipNoNewCommits`, or `Unknown`.
   - First run (`last_review_ts == None`) returns `Run`.
2. Implement the gate through an isolated repository-query boundary rather than
   embedding process spawning in `periodic_reviewer.rs`.
   - Do not add direct `Command::new("git")` text to `periodic_reviewer.rs`.
   - Reuse the existing sanitized git execution conventions if the
     implementation needs a local git query.
   - Keep command arguments as arrays, never shell strings.
3. On `SkipNoNewCommits`, return before `ensure_review_queue_limit`,
   `CreateTaskRequest`, and `enqueue_task_background_in_domain`.
   - Emit structured tracing with project name/root, watermark, and reason.
   - Log an EventStore event or equivalent observable signal that does not
     advance the successful-review watermark past the checked bound.
4. On `Unknown`, preserve review coverage.
   - Log the failure visibly.
   - Fall back to the current prompt-side `REVIEW_SKIPPED` behavior so the
     scheduler does not skip potentially changed code.
5. Keep agent-side `REVIEW_SKIPPED` handling unchanged.
   - It still protects races, first-run anomalies, and local gate failures.
   - Existing exact-match sentinel detection must stay exact-match only.
6. Update docs only where needed.
   - `docs/usage-guide.md` and `docs/periodic-review.md` should describe the
     local gate and fallback behavior accurately.

## Data Flow

`run_review_tick` queries EventStore, merges `ReviewState::fallback_ts`, and
gets the effective watermark. Before constructing `base_prompt`, it asks the
local gate whether the project has commits after that watermark. If there are
none, the function logs the local skip and returns `Ok(())`. If there are new
commits or the gate cannot determine the answer, the existing enqueue and poll
flow continues.

Successful review parsing continues to write the normal `periodic_review`
watermark after real review output is available. Local no-change skips should
not move that successful-review watermark forward in a way that could hide a
commit that appears after the check.

## Alternatives Considered

- Rely only on the agent prompt. Rejected because this is the current behavior
  and is the source of the quota waste.
- Advance the `periodic_review` watermark on every local skip. Rejected because
  `periodic_review` currently means a completed review checkpoint; advancing it
  on no-op skips risks changing downstream assumptions.
- Remove the prompt-side sentinel. Rejected because it is a useful safety net
  for races and local-gate failures.
- Add remote GitHub polling. Rejected because the requirement is a cheap local
  gate against the registered project workspace.

## Risks

- A broken local gate could skip needed review work. Mitigate by treating
  uncertainty as `Unknown` and falling back to agent-side review.
- Commit timestamps can be imprecise around boundary times. Mitigate by using a
  deterministic “newer than watermark” query and keeping the agent fallback.
- Tests around `run_review_tick` may need dependency injection to avoid a real
  agent queue. Keep the seam narrow and test the gate plus enqueue decision.

## Test Plan

- [ ] Unit-test the local gate: no watermark runs, empty commit query skips, and
      newer commit query runs.
- [ ] Test unchanged project with prior watermark produces zero primary review
      enqueues.
- [ ] Test project with newer commit enqueues primary review work.
- [ ] Test gate failure falls back to enqueue path and logs the failure.
- [ ] Keep `test_git_guard_removed` or equivalent structural coverage for
      `periodic_reviewer.rs`.
- [ ] Run `cargo test -p harness-server periodic_reviewer`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1470`.
- [ ] Run `python3 checks/check_workflow.py --repo .`.
