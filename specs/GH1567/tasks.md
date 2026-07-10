# Task Plan

## Linked Issue

GH-1567

## Spec Packet

- Product: `specs/GH1567/product.md`
- Tech: `specs/GH1567/tech.md`

## Implementation Tasks

- [x] `SP1567-T001` Owner: `workflow-runtime` | Dependencies: none | Done when: blocked and failed reducer paths derive structured stop metadata (`blocked_reason`, `unblock_hint`, `failure_reason`, `retry_hint`, and `last_stop`) from activity results and persist it in workflow instance `data` without schema migration | Verify: `cargo test -p harness-workflow runtime_failure`
- [x] `SP1567-T002` Owner: `server-api` | Dependencies: `SP1567-T001` | Done when: `/api/workflows/runtime/unblock` exists, is authenticated by existing middleware, accepts `workflow_id` and `reason`, accepts only `blocked` workflows, records an audit event, and moves the workflow to a dispatchable state | Verify: `cargo test -p harness-server unblock`
- [x] `SP1567-T003` Owner: `server-api` | Dependencies: `SP1567-T001` | Done when: `/api/workflows/runtime/retry` exists, is authenticated by existing middleware, accepts only retryable `failed` workflows, rejects wrong-state and non-retryable failures, records an audit event, and moves the workflow to a dispatchable state | Verify: `cargo test -p harness-server retry`
- [ ] `SP1567-T004` Owner: `intake` | Dependencies: `SP1567-T002`, `SP1567-T003` | Done when: GitHub issue coverage remains conservative for untouched `blocked`/`failed` workflows, but a workflow updated by unblock or retry no longer causes the next intake tick to skip the issue because of the old stopped state | Verify: `cargo test -p harness-server github_coverage_gate`
- [ ] `SP1567-T005` Owner: `observe` | Dependencies: `SP1567-T001` | Done when: runtime tree and operator monitor payloads expose structured stopped-state fields and action eligibility without relying on free-text parsing | Verify: `cargo test -p harness-server operator_monitor runtime_tree`
- [ ] `SP1567-T006` Owner: `web` | Dependencies: `SP1567-T002`, `SP1567-T003`, `SP1567-T005` | Done when: the operator monitor renders structured reason text, shows unblock/retry actions only when allowed, calls the new routes, and refreshes state after success | Verify: project web test command for `OperatorMonitorPanel`
- [ ] `SP1567-T007` Owner: `docs` | Dependencies: `SP1567-T002`, `SP1567-T003` | Done when: `docs/usage-guide.md` documents the recovery API, response classes, supported states, and the rule that direct DB edits are not supported recovery | Verify: reviewer checks usage-guide section
- [ ] `SP1567-T008` Owner: `policy` | Dependencies: all previous tasks | Done when: optional blocked recheck policy is either explicitly deferred or implemented behind disabled-by-default configuration with evidence-writing tests | Verify: implementation PR states the decision and matching tests or non-goal evidence

## Parallelization

- Lane A (`SP1567-T001`) is the first prerequisite and should land alone or with
  minimal serialization tests.
- Lane B (`SP1567-T002`, `SP1567-T003`) can be split into two server API PRs
  after Lane A. They share route and store helpers, so do not run writable work
  on the same files in parallel.
- Lane C (`SP1567-T004`, `SP1567-T005`) follows the API semantics and can be
  reviewed separately if file ownership stays disjoint.
- Lane D (`SP1567-T006`, `SP1567-T007`) can proceed after API response shapes
  stabilize.
- `SP1567-T008` is a final policy decision and should not block the manual
  operator escape hatch.

## Verification

- `python3 checks/check_workflow.py --repo .`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1567`
- `python3 checks/route_gate.py --repo . --route implement --issue 1567 --state ready_to_implement --json`
- For this spec-only PR: `cargo fmt --all -- --check`
- For implementation PRs: run focused package tests from the task rows, then
  run broader workspace checks when shared runtime behavior changes.

## Handoff Notes

- This spec intentionally keeps the first implementation manual/operator-driven.
  Automatic blocked recheck is deferred unless a follow-up PR explicitly
  enables it behind disabled-by-default configuration.
- Use body-based runtime action routes for the first implementation because
  current workflow ids can contain project paths and slashes. A path-style alias
  requires URL-encoding tests.
- Do not make `blocked` or `failed` globally uncovered in the coverage gate.
  The operator action should change the workflow state; the gate should then
  observe that updated state.
- Do not retry `cancelled` workflows in this issue.
- Do not close GH-1567 until the implementation PRs satisfy the acceptance
  criteria, not merely this spec packet.
