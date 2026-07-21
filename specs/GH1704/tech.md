# Tech Spec

## Linked Issue

GH-1704

## Product Spec

See `specs/GH1704/product.md`.

<!-- specrail-planned-changes
{"issue":1704,"complete":true,"paths":["crates/harness-server/src/handlers/mod.rs","crates/harness-server/src/handlers/runtime_hosts.rs","crates/harness-server/src/handlers/runtime_hosts_workflow_api_tests.rs","crates/harness-server/src/http/http_router.rs","crates/harness-server/src/http/task_mutation_routes.rs","crates/harness-server/src/http/tests/mod.rs","crates/harness-server/src/http/tests/runtime_transcript_route_tests.rs","crates/harness-server/src/workflow_runtime_worker.rs","crates/harness-server/src/workflow_runtime_worker/executor.rs","crates/harness-server/src/workflow_runtime_worker/executor_contract.rs","crates/harness-server/src/workflow_runtime_worker/transcript_durability.rs","crates/harness-workflow/src/runtime/mod.rs","crates/harness-workflow/src/runtime/reason_class.rs","crates/harness-workflow/src/runtime/reducer/prompt_task_completion.rs","crates/harness-workflow/src/runtime/reducer/runtime_failure.rs","crates/harness-workflow/src/runtime/store.rs","crates/harness-workflow/src/runtime/store/activity_completion.rs","crates/harness-workflow/src/runtime/store/artifacts.rs","crates/harness-workflow/src/runtime/store/commands.rs","crates/harness-workflow/src/runtime/store/instances.rs","crates/harness-workflow/src/runtime/store/runtime_jobs.rs","crates/harness-workflow/src/runtime/store_migrations.rs","crates/harness-workflow/src/runtime/tests.rs","crates/harness-workflow/src/runtime/tests/runtime_failure_classification.rs","crates/harness-workflow/src/runtime/tests/transcript_durability.rs","crates/harness-workflow/src/runtime/transcript.rs","crates/harness-workflow/src/runtime/worker.rs","docs/api-contract.md"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010"]}
-->

## Current System

- `crates/harness-workflow/src/runtime/store/runtime_completion.rs:435`
  validates other durable continuation evidence during completion, but the
  base tree has no equivalent raw-transcript durability contract.
- `crates/harness-workflow/src/runtime/store_migrations.rs:85` creates generic
  workflow artifacts; replay-required transcript bytes need stronger identity,
  checksum, retention, and producer constraints.
- `crates/harness-workflow/src/runtime/worker.rs:413` filters server-generated
  artifacts at the worker boundary, so transcript references must be created by
  the trusted runtime path rather than accepted from agent-authored payloads.
- Runtime-host and local execution reach completion through different server
  adapters and must converge before the workflow store acknowledges success.

## Proposed Design

Add a typed transcript-evidence model and a workflow-store persistence surface
backed by an additive migration. The completion transaction inserts or verifies
the transcript, records content-derived identity, binds the producer job,
stores the activity result/event, and applies the reducer decision atomically.

Exact-replay dispatch performs a store preflight that loads bytes, verifies
size/checksum/ownership, and only then hydrates the local or `RemoteHost`
request. Transient storage-class failures map to the existing bounded retry
policy; missing/corrupt/ownership failures map to stable terminal reason codes.

Retention queries operate on the complete workflow dependency family. GC may
delete transcript rows only when every family member is terminal. An
authenticated bounded reconstruction endpoint accepts provider-exported bytes,
verifies the expected identity contract, and atomically restores the artifact.

Transcript read and reconstruction routes are sensitive endpoints. Route
construction must require an effective endpoint-specific authenticated mode.
When the server resolves to open API mode through
`allow_unauthenticated = true`, those endpoints fail closed as unavailable
before request-body parsing or store access; presenting an arbitrary bearer
header does not activate them. This restriction does not silently change the
availability contract of unrelated open-mode routes.

## Data Flow

`agent/RemoteHost output -> trusted transcript extraction -> checksum/size ->
completion transaction (transcript + job + event + decision) -> durable
reference -> dependency-aware retention -> exact-replay preflight -> hydrated
consumer`.

Recovery uses `endpoint-specific authenticated authorization -> bounded
re-export body -> identity/ownership validation -> atomic insert/verify ->
subsequent preflight`. Open API mode stops before the bounded body enters this
flow.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | activity completion transaction and transcript store | `cargo test -p harness-workflow transcript_completion_is_atomic` |
| B-002 | transcript model/store constraints | checksum, size, and producer ownership tests under `runtime::tests::transcript_durability` |
| B-003 | dependency-aware retention query | `cargo test -p harness-workflow 'runtime::tests::transcript_durability::transcript_retention_waits_for_every_dependent_workflow_to_finish' -- --exact` |
| B-004 | worker preflight dispatch suppression and verified hydration | `cargo test -p harness-server 'http::tests::runtime_transcript_route_tests::exact_replay_preflight_fails_terminal_on_missing_or_corrupt_transcript' -- --exact` and `cargo test -p harness-server 'http::tests::runtime_transcript_route_tests::exact_replay_hydrates_verified_transcript_before_dispatch' -- --exact` |
| B-005 | reason classification and reducer retry | `cargo test -p harness-workflow 'runtime::tests::runtime_transcript_store_failure_uses_bounded_default_backoff' -- --exact` and `cargo test -p harness-workflow 'runtime::tests::runtime_transcript_store_failure_honors_explicit_zero_retry_limits' -- --exact` |
| B-006 | terminal failure mapping and active-queue projection | `cargo test -p harness-workflow 'runtime::tests::confirmed_runtime_transcript_loss_is_terminal' -- --exact`, plus missing/corrupt transcript integration tests and server projection tests |
| B-007 | reconstruction handler/store operation | `cargo test -p harness-server runtime_transcript_route` |
| B-008 | local executor and runtime-host handlers | executor contract and runtime-host transcript tests |
| B-009 | transaction rollback and restart | transcript rollback/reopen tests under `runtime::tests::transcript_durability` |
| B-010 | endpoint-specific authenticated bounded HTTP routes | reconstruction route tests prove enforced-mode success and prove open API mode is unavailable before body parsing or store mutation; `cargo test -p harness-server runtime_transcript_route` |

## Alternatives Considered

- Keep filesystem paths in workflow evidence: rejected because paths are not
  stable across hosts or retention/restore boundaries.
- Store only a checksum and retrieve provider history on demand: rejected
  because exact replay would remain unavailable during provider outages.
- Warn and replay a summary: rejected because it violates exact-replay intent
  and silently changes workflow output.
- Retain all transcripts forever: rejected because dependency-aware pinning
  provides the required lifetime without unbounded storage.
- Reconstruct without authentication: rejected because transcripts may contain
  sensitive source and execution context.

## Risks

- Security: transcript bytes are sensitive; read/reconstruction routes require
  endpoint-specific authenticated authorization, remain unavailable in open
  API mode, enforce bounded bodies, and return sanitized errors. Because this
  changes an authentication boundary, SEC-11 requires a human security review
  of implementation PR #1710 at its exact merge head.
- Data integrity: completion must never commit a reference without verified
  bytes or accept an agent-forged server reference.
- Compatibility: the migration is additive, but historical workflows lacking
  bytes cannot pretend to satisfy exact replay.
- Performance: checksum and persistence add bounded I/O to completion; large
  bodies must respect explicit server limits.
- Maintenance: local and `RemoteHost` paths must share the same typed contract
  to prevent drift.

## Test Plan

- [ ] Run every Product-to-Test Mapping command/filter.
- [ ] Run both exact B-004 selectors: the missing/corrupt case must prove the
      consumer is not dispatched, and the verified case must prove transcript
      hydration completes before dispatch.
- [ ] Run `cargo check --workspace --all-targets`.
- [ ] Run `cargo fmt --all -- --check` and
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run PostgreSQL-backed workflow/server suites with an isolated
      `HARNESS_DATABASE_URL`, including restart and retention/GC cases.
- [ ] Prove transcript read/reconstruction succeeds only under enforced
      endpoint-specific authentication and remains unavailable in
      `allow_unauthenticated = true` open API mode without parsing or mutation.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1704`.
- [ ] Collect exact-head CI, Gemini review, independent reviewer evidence,
      GraphQL review-thread state, and SpecRail PR-gate evidence.
- [ ] Collect named human security-review approval for PR #1710 at the exact
      merge head. This blocking SEC-11 evidence cannot be supplied by an agent,
      independent AI reviewer lane, or implx auto standing authorization.

## Rollback Plan

Squash-revert server/workflow behavior while leaving the additive transcript
table in place until a later safe migration. Disable exact-replay dispatch if a
rollback would otherwise consume references it can no longer validate. Never
drop transcript data while non-terminal dependency families remain.
