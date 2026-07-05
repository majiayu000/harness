# Product Spec

## Linked Issue

GH-1470

## User Problem

The periodic reviewer currently enqueues the primary review agent on each due
tick and asks the agent to run `git log --since=<last_review>` from the prompt.
When no commits changed, the agent returns `REVIEW_SKIPPED`, but the system has
already spent one agent invocation.

For idle projects this turns every periodic tick into quota and queue pressure.
At large project counts, the default cadence can create thousands of no-op
agent invocations per day.

## Goals

- Add a cheap local gate before primary periodic review enqueue.
- Skip primary, secondary, and synthesis agent work when the project has no
  commits newer than the last completed periodic review watermark.
- Keep the existing agent-side `REVIEW_SKIPPED` sentinel as defense in depth.
- Record local skips observably so operators can tell a tick was intentionally
  skipped rather than silently dropped.
- Preserve the current review cadence, review content, and successful-review
  watermark behavior when new commits exist.

## Non-Goals

- Changing periodic review interval semantics.
- Changing the periodic review prompt checklist beyond aligning skip wording
  with the new local gate.
- Removing agent-side `REVIEW_SKIPPED` handling.
- Adding network GitHub checks or remote branch polling.
- Reworking cross-review, synthesis, or auto-fix task spawning.

## User-Visible Behavior

When a project has a prior completed periodic review watermark and no newer
local commits, the scheduler should log or emit a local skip and should not
enqueue the primary review task. Cross-review and synthesis tasks should not be
created for that tick.

When the project has no prior watermark, the first eligible tick should still
run. When the project has at least one local commit newer than the watermark,
the scheduler should enqueue the primary periodic review task exactly as it
does today.

## Acceptance Criteria

- [ ] An unchanged project with a prior periodic review watermark produces zero
      primary agent task enqueues for that tick.
- [ ] A project with a commit newer than the watermark still enqueues the
      primary periodic review task.
- [ ] First-run projects without a watermark still enqueue review work.
- [ ] Local skips are observable through structured tracing and/or EventStore
      data that names the project and skip reason.
- [ ] Agent-side `REVIEW_SKIPPED` handling remains intact for defense in depth.
- [ ] The implementation does not add a direct `Command::new("git")` call in
      `periodic_reviewer.rs`.

## Edge Cases

- If the local commit check fails, the scheduler should fail the tick visibly
  or log a warning and fall back to the current agent-side skip path; it must
  not silently drop review work.
- The local gate should use the same effective watermark as the prompt:
  EventStore timestamp merged with the in-memory fallback timestamp.
- Path-specific watermark keys must remain isolated per project root.
- Commits that arrive while a review is running should remain covered by the
  existing watermark-bound logic after the review completes.

## Rollout Notes

Use `Refs #1470` for the spec PR. The implementation PR should use
`Closes #1470` after the local gate, observability, docs, and tests land.
