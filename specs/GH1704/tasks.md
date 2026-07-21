# Task Plan

## Linked Issue

GH-1704

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1704-T1` Owner: workflow-runtime implementation agent | Done when: transcript evidence and completion commit atomically | Verify: atomicity, checksum, ownership, and restart tests below
- [ ] `SP1704-T2` Owner: workflow-runtime implementation agent | Done when: retention and exact-replay preflight fail closed | Verify: retention, GC, retry, and terminal-classification tests below
- [ ] `SP1704-T3` Owner: harness-server implementation agent | Done when: local and RemoteHost completion share one durable contract | Verify: executor and runtime-host tests below
- [ ] `SP1704-T4` Owner: harness-server implementation agent | Done when: authenticated reconstruction safely restores valid evidence | Verify: reconstruction route tests below
- [ ] `SP1704-T5` Owner: coordinator | Done when: all local and exact-head remote gates pass | Verify: workspace, CI, review-thread, and PR-gate commands below

### SP1704-T1 — Define and migrate durable transcript evidence

- Owner: workflow-runtime implementation agent.
- Dependencies: merged GH-1704 spec PR.
- Covers: B-001, B-002, B-009.
- Work: add the typed evidence contract, additive store migration, producer
  ownership, checksum/size constraints, and atomic completion integration.
- Done when: rollback cannot expose partial evidence or acknowledged completion,
  and restart can read committed bytes by stable reference.
- Verify: atomicity, ownership, checksum, rollback, and reopen tests from
  `tech.md`.

### SP1704-T2 — Enforce retention and exact-replay preflight

- Owner: workflow-runtime implementation agent.
- Dependencies: SP1704-T1.
- Covers: B-003, B-004, B-005, B-006, B-009.
- Work: pin by workflow family, verify evidence before dispatch, hydrate replay,
  and map transient versus terminal failures without active-queue ambiguity.
- Done when: GC cannot remove live dependencies, invalid evidence never starts
  a consumer, and retries/terminal outcomes are deterministic.
- Verify: retention/GC, missing/corrupt/read failure, retry, and projection tests.

### SP1704-T3 — Unify local and RemoteHost transcript completion

- Owner: harness-server implementation agent.
- Dependencies: SP1704-T1 and SP1704-T2.
- Covers: B-001, B-002, B-004, B-008, B-010.
- Work: route both execution surfaces through the trusted durable contract and
  enforce request authentication/body limits.
- Done when: both producers create equivalent evidence and large/invalid bodies
  follow the same bounded fail-closed behavior.
- Verify: executor-contract, runtime-host workflow API, and HTTP route tests.

### SP1704-T4 — Add authenticated reconstruction

- Owner: harness-server implementation agent.
- Dependencies: SP1704-T1.
- Covers: B-002, B-007, B-010.
- Work: accept provider re-export under API authentication, validate identity
  and checksum, atomically import, and prove repeat safety.
- Done when: valid reconstruction restores preflight and invalid/unauthorized
  requests make no durable change.
- Verify: reconstruction success, auth, mismatch, oversized, and repeat tests.

### SP1704-T5 — Complete repository and PR gates

- Owner: coordinator; independent reviewer owns review verdict.
- Dependencies: SP1704-T2 through SP1704-T4.
- Covers: B-001 through B-010.
- Work: run workspace checks, PostgreSQL suites, SpecRail checks, exact-head CI,
  Gemini, independent review, thread audit, and PR gate.
- Done when: all current evidence is green and the implementation merges under
  standing authorization without an unresolved thread.
- Verify: commands in `tech.md` and exact-head GitHub evidence.

## Parallelization

After SP1704-T1 fixes the shared contract, local/RemoteHost integration and the
reconstruction route may use disjoint writable lanes. Store migration,
completion transaction, and retention logic remain under one owner.

## Verification

- [ ] Product invariant set equals task coverage union: B-001 through B-010.
- [ ] Global and GH-1704 SpecRail checks pass.
- [ ] Workflow and server focused tests pass with an isolated database.
- [ ] Workspace format/check/clippy/tests and exact-head CI pass.
- [ ] Gemini and independent review have no unresolved active finding.
- [ ] PR gate is `allowed` at the exact merge head.

## Handoff Notes

- The Remem replay incident is the acceptance anchor; mitigation-only runtime
  retry and queue-projection changes do not satisfy this issue.
- Exact replay must never silently degrade to summary replay.
- The implementation PR is #1710 and must remain blocked until this heavy-tier
  spec PR is merged and its exact-head review evidence is current.
