# AGENTS.md — Harness Ticket Execution Workflow

This repository is used in unattended orchestration sessions. Follow this document for ticket execution unless a higher-priority instruction (system/developer/user) overrides it.

## Core Operating Mode

- Work autonomously end-to-end; do not ask humans for routine follow-up.
- Stop early only for true blockers (missing required auth/permissions/secrets/tools).
- Work only inside the current repository copy.
- Keep one persistent Linear workpad comment as the execution source of truth.
- Keep issue metadata, validation evidence, and status transitions accurate at all times.

## Tooling Preferences

- Prefer built-in tracker operations for issue reads/updates/comments when available.
- If using `linear_graphql`, run targeted introspection first for unfamiliar fields/arguments.
- Avoid schema guessing; use focused queries/mutations.

## Ticket State Machine

Determine issue state first and route accordingly:

- `Backlog`: do not modify issue content/state; wait for move to `Todo`.
- `Todo`: immediately move to `In Progress`, then bootstrap/reconcile workpad, then execute.
- `In Progress`: continue execution from the existing workpad.
- `Human Review`: do not code or alter implementation; only poll for decision/feedback.
- `Merging`: follow `.codex/skills/land/SKILL.md` and complete landing loop.
- `Rework`: reset approach (close PR, remove old workpad, fresh branch from `origin/main`, restart flow).
- `Done`: terminal; no further action.

## Mandatory Startup Flow

1. Fetch issue by explicit ID.
2. Read current state and route by the state machine above.
3. Check branch PR status:
   - If existing branch PR is `CLOSED`/`MERGED`, do not reuse prior branch work.
   - Create a fresh branch from `origin/main` and restart from kickoff.
4. For `Todo` tickets, strict order:
   1) move issue to `In Progress`
   2) find/create `## Codex Workpad` comment
   3) begin analysis/planning/implementation
5. Add a short comment only if issue state and issue content are inconsistent.

## Workpad Rules (Single Comment Only)

- Reuse existing unresolved `## Codex Workpad` comment if present.
- Otherwise create one, then update that same comment throughout execution.
- Never post separate progress/done summary comments outside this workpad.
- Keep this environment stamp at top:

```text
<hostname>:<abs-workdir>@<short-sha>
```

- Include and maintain:
  - hierarchical checklist plan
  - acceptance criteria checklist
  - validation checklist
  - timestamped notes
  - `Confusions` section when anything is unclear

## Planning and Reproduction Requirements

Before implementation:

- Reconcile workpad checklist against current reality.
- Expand plan to fully cover ticket scope.
- Mirror any ticket-authored `Validation`/`Test Plan`/`Testing` sections as required checkboxes.
- Run a principal-style self-review of the plan and refine it.
- Capture a concrete reproduction signal (command output, deterministic behavior, or screenshot) in workpad notes.
- Sync with `origin/main` before edits and record pull evidence:
  - merge source(s)
  - result (`clean` or `conflicts resolved`)
  - resulting HEAD short SHA

## Implementation Rules

- Execute against the workpad checklist; keep it updated after each meaningful milestone.
- Check completed items immediately; add newly discovered scoped tasks where appropriate.
- Keep parent/child checklist structure coherent as scope evolves.
- Implement only in-scope changes; open separate follow-up issues for meaningful out-of-scope improvements.
- If app/runtime behavior is touched, include explicit launch/interaction/result acceptance checks.

## Validation Rules

- Required gate: execute all ticket-provided validation/test-plan requirements.
- Run targeted proof first, then broader checks as needed.
- Temporary local proof edits are allowed only for validation and must be reverted before commit.
- Record validation commands/results in workpad.
- Before every push: rerun required validation and ensure it is green.
- If app-touching, run app runtime validation and attach required media artifacts before handoff.

## PR and Feedback Sweep Protocol

When a PR exists (including pre-existing PRs):

1. Gather all feedback channels:
   - top-level PR comments
   - inline review comments
   - review summaries/states
2. Treat every actionable reviewer comment (human or bot) as blocking until:
   - code/test/docs are updated, or
   - an explicit, justified pushback reply is posted on that thread.
3. Track each feedback item and resolution status in the workpad.
4. Re-run validation after feedback-driven changes.
5. Repeat until no outstanding actionable comments remain.

Additional PR requirements:

- Attach PR URL to the Linear issue via attachment/link field (not in workpad body unless attachment is unavailable).
- Ensure PR has label `symphony`.
- Merge latest `origin/main` into branch before handoff and rerun checks.

## Blocked-Access Escape Hatch

Use only for true blockers (missing required non-GitHub tools/auth/permissions/secrets).

- GitHub access alone is not a blocker until fallback strategies are exhausted.
- If blocked, record concise blocker brief in workpad:
  - what is missing
  - why it blocks required acceptance/validation
  - exact human unblock action
- Move issue state according to workflow only after documenting blocker in workpad.

## State Transition Quality Bar

Move to `Human Review` only when all are true:

- workpad plan/checklists accurately reflect completed work
- acceptance criteria complete
- required validation/test-plan items complete
- latest validations/checks are green
- PR feedback sweep is fully resolved
- PR is pushed, linked to issue, and labeled `symphony`
- app runtime/media requirements completed when applicable

In `Human Review`:

- do not make code or ticket-content changes
- poll for updates
- if changes requested, move to `Rework` and restart rework flow

In `Merging`:

- execute land skill loop until merged, then move issue to `Done`

## Rework Reset Protocol

For `Rework` state:

1. Re-read full issue + all human comments.
2. Close current PR tied to the issue.
3. Remove prior `## Codex Workpad` comment.
4. Create fresh branch from `origin/main`.
5. Restart normal kickoff flow with a new workpad.

## Guardrails

- Do not edit issue body for planning/progress tracking.
- Do not use multiple live workpad comments.
- Do not leave completed tasks unchecked in workpad.
- Do not move to `Human Review` before all quality gates pass.
- Do not continue coding while issue is in `Human Review`.
- Keep all updates concise, specific, reviewer-oriented.

## Workpad Template

Use and maintain this exact structure:

````md
## Codex Workpad

```text
<hostname>:<abs-path>@<short-sha>
```

### Plan

- [ ] 1\. Parent task
  - [ ] 1.1 Child task
  - [ ] 1.2 Child task
- [ ] 2\. Parent task

### Acceptance Criteria

- [ ] Criterion 1
- [ ] Criterion 2

### Validation

- [ ] targeted tests: `<command>`

### Notes

- <short progress note with timestamp>

### Confusions

- <only include when something was confusing during execution>
````
