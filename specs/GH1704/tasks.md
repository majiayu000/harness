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
- [ ] `SP1704-T5` Owner: coordinator and named human security reviewer | Done when: all local and exact-head remote gates pass, including human SEC-11 approval | Verify: workspace, CI, review-thread, human-security-review, and PR-gate evidence below

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
  a consumer, verified evidence hydrates the consumer before dispatch, and
  retries/terminal outcomes are deterministic.
- Verify: retention/GC, retry, and projection tests, plus
  `cargo test -p harness-server 'http::tests::runtime_transcript_route_tests::exact_replay_preflight_fails_terminal_on_missing_or_corrupt_transcript' -- --exact`
  for dispatch suppression and
  `cargo test -p harness-server 'http::tests::runtime_transcript_route_tests::exact_replay_hydrates_verified_transcript_before_dispatch' -- --exact`
  for successful pre-dispatch hydration.

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
  and checksum, atomically import, prove repeat safety, and keep transcript
  read/reconstruction unavailable when the server is in
  `allow_unauthenticated = true` open API mode without endpoint-specific
  authenticated authorization.
- Done when: valid reconstruction restores preflight and invalid/unauthorized
  requests make no durable change; open API mode stops before request-body
  parsing or transcript-store access.
- Verify: reconstruction success, enforced-auth, open-mode unavailable,
  mismatch, oversized, no-mutation, and repeat tests.

### SP1704-T5 — Complete repository and PR gates

- Owner: coordinator for mechanical gates; a named human security reviewer owns
  the SEC-11 verdict.
- Dependencies: SP1704-T2 through SP1704-T4.
- Covers: B-001 through B-010.
- Work: run workspace checks, PostgreSQL suites, SpecRail checks, exact-head CI,
  Gemini, independent review, thread audit, PR gate, and mandatory human
  security review of implementation PR #1710.
- Done when: all current evidence is green, no active review thread remains,
  and a named human security reviewer has approved the exact merge head. implx
  auto standing authorization, queue-drain authorization, Gemini, and agent
  review do not satisfy the SEC-11 gate.
- Verify: commands in `tech.md`, exact-head GitHub evidence, and a recorded
  human security-review identity, verdict, reviewed head SHA, and timestamp.

## Parallelization

After SP1704-T1 fixes the shared contract, local/RemoteHost integration and the
reconstruction route may use disjoint writable lanes. Store migration,
completion transaction, and retention logic remain under one owner.

## Verification

- [ ] Product invariant set equals task coverage union: B-001 through B-010.
- [ ] Global and GH-1704 SpecRail checks pass.
- [ ] Workflow and server focused tests pass with an isolated database.
- [ ] B-004 exact tests prove both invalid-evidence dispatch suppression and
      verified-evidence hydration before dispatch.
- [ ] Exact `cargo test -- --list` names are used for retention and runtime
      transcript failure-classification coverage.
- [ ] Workspace format/check/clippy/tests and exact-head CI pass.
- [ ] Gemini and independent review have no unresolved active finding.
- [ ] A named human security reviewer approves PR #1710 at the exact merge head;
      implx auto authorization is explicitly not counted as this evidence.
- [ ] PR gate is `allowed` at the exact merge head.

## Handoff Notes

- The Remem replay incident is the acceptance anchor; mitigation-only runtime
  retry and queue-projection changes do not satisfy this issue.
- Exact replay must never silently degrade to summary replay.
- The implementation PR is #1710 and must remain blocked until this heavy-tier
  spec PR is merged and its exact-head review evidence is current.
- PR #1710 must also remain blocked until mandatory SEC-11 human security review
  is recorded at the exact merge head. No auto-mode standing authorization or
  AI reviewer verdict can replace that evidence.
