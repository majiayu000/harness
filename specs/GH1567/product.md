# Product Spec

## Linked Issue

GH-1567

## User Problem

Operators have no supported way to recover a workflow-runtime issue instance
after it reaches `blocked`, `failed`, or `cancelled`.

In the intake path, `runtime_issue_state_is_covered` treats those states as
covered, so GitHub issue polling skips the issue forever. If a workflow blocks
because it is waiting for maintainer input, and the maintainer later provides
that input on the issue, Harness does not re-read the issue thread and does not
dispatch follow-up work. The only known recovery path from the reported
production incident was manual SQL deletion across workflow runtime tables,
which is not acceptable operator behavior.

## Goals

1. Make blocked and failed workflow-runtime instances explain why they stopped
   using structured, API-visible fields.
2. Provide operator-authenticated HTTP actions to unblock blocked instances and
   retry failed instances without direct Postgres edits.
3. Ensure a successful unblock makes the related GitHub issue eligible for the
   next intake tick instead of remaining covered forever.
4. Document the supported recovery flow so operators do not rely on table
   surgery.
5. Keep automatic recovery conservative by default. A blocked workflow must not
   resume unless an operator acts or a repository explicitly opts into a
   recheck policy.

## Non-Goals

- No automatic retry of `blocked`, `failed`, or `cancelled` workflows by
  default.
- No new SpecRail-specific state machine inside Harness core. Repository policy
  remains in the repository layer.
- No direct mutation of GitHub labels, issue state, or comments from the
  recovery API.
- No deletion of workflow runtime rows as part of recovery.
- No change to PR merge approval semantics.

## User-Visible Behavior

An operator viewing a stopped workflow can see:

- the workflow state;
- a stable `blocked_reason` or `failure_reason`;
- a short `unblock_hint` or `retry_hint` when Harness can suggest the next
  operator action;
- the activity and runtime job that produced the stopped state when available.

An operator can call an authenticated HTTP action to:

- move a `blocked` workflow back into a dispatchable state after external input
  has been provided;
- retry a `failed` workflow when the failure is transient or after the operator
  has corrected the external condition.

After a successful unblock or retry, the next GitHub issue intake scan must not
skip the issue solely because the previous workflow state was `blocked` or
`failed`. The workflow should either dispatch new work or report a fresh,
structured blocker.

If an operator calls the wrong action for the current state, Harness returns a
clear conflict response instead of silently doing nothing.

## Acceptance Criteria

- [ ] Blocked workflow-runtime instances expose `blocked_reason`,
      `unblock_hint`, and source metadata through the runtime tree and operator
      monitor APIs.
- [ ] Failed workflow-runtime instances expose `failure_reason`, `error_kind`,
      `retry_hint`, and source metadata through the same operator-visible
      surfaces.
- [ ] An authenticated operator can unblock a `blocked` workflow through HTTP
      without editing Postgres directly.
- [ ] An authenticated operator can retry a `failed` workflow through HTTP
      without editing Postgres directly.
- [ ] A successful unblock or retry records an audit event and updates the
      workflow instance so the GitHub issue coverage gate no longer treats the
      old terminal state as a permanent hold.
- [ ] The next intake tick for the same issue can re-dispatch work or produce a
      fresh structured blocker.
- [ ] `cancelled` workflows are not automatically retried. Any support for
      cancelled workflows must be an explicit follow-up with separate
      acceptance criteria.
- [ ] `docs/usage-guide.md` documents the recovery API, expected responses, and
      the rule that direct DB edits are not part of the supported flow.

## Edge Cases

- The workflow runtime store is unavailable.
- The workflow id is not found.
- The workflow is already active or terminal in a state that the requested
  action does not support.
- A blocked workflow has no structured reason because it was created before
  this feature shipped.
- A failed workflow has a non-retryable `error_kind` such as `fatal` or
  `configuration`.
- The related GitHub issue is closed between the original block and the
  operator recovery action.
- The intake tick races with the operator action.

## Rollout Notes

This feature changes operator recovery behavior and should land in small PRs:

1. structured stop-reason metadata;
2. HTTP unblock/retry actions;
3. coverage-gate and intake re-dispatch behavior;
4. dashboard and usage-guide documentation.

Existing blocked or failed rows may not have all structured fields. The UI and
API must tolerate missing fields and fall back to existing event or activity
summary text without fabricating reasons.
