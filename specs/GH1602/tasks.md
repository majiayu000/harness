# GH1602 Task Plan

## Linked Issue

GH-1602

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1602-T001` Owner: `server-lease-contract` | Dependencies: none | Covers: B-005, B-006, B-012, B-014 | Done when: one server-owned lease policy defines the 60-second default and inclusive `1..=3600` range; claim and renewal request parsing reject zero, oversized, malformed, null, and overflow-inducing values without clamping; claim responses add `lease_generation`; renewal response/error DTOs expose only the documented stable fields and never competing-owner evidence | Evidence: focused parser and serialization tests record all accepted boundaries and rejection classes | Verify: `cargo test -p harness-server runtime_job_lease_duration` and `cargo test -p harness-server runtime_hosts_workflow_api`
- [ ] `SP1602-T002` Owner: `workflow-persistence-schema` | Dependencies: none | Covers: B-004, B-007, B-011, B-014 | Done when: additive migrations and workflow model types represent renewal receipts keyed by runtime job, generation, and renewal ID plus the closed claim/reclaim/renew/reject/revoke audit events; legacy runtime-job JSON remains readable without fabricating receipts | Evidence: migration/model tests prove legacy deserialization, receipt uniqueness, replay input storage, and ordered event serialization | Verify: `cargo test -p harness-workflow runtime_store_remote_host_lease` and `cargo test -p harness-workflow runtime_model`
- [ ] `SP1602-T003` Owner: `workflow-lease-order` | Dependencies: `SP1602-T002` | Covers: B-004, B-011, B-015 | Done when: transaction-local event append and receipt-cleanup helpers exist; RemoteHost claim/reclaim mutation, generation increment, prior-generation receipt cleanup, and `RuntimeJobClaimed`/`RuntimeJobReclaimed` evidence commit atomically; completion, cancellation, and revocation paths clear current-generation receipts inside their ownership-ending transaction | Evidence: rollback/failpoint tests show no lease mutation without its event, no success event without mutation, same-host reclaim increments generation, and ending a generation removes its receipts | Verify: `cargo test -p harness-workflow runtime_store_remote_host_lease`
- [ ] `SP1602-T004` Owner: `workflow-renewal-transaction` | Dependencies: `SP1602-T001`, `SP1602-T002`, `SP1602-T003` | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-011, B-013, B-015 | Done when: one row-locked PostgreSQL transaction accepts explicit server `now`, validates running RemoteHost state plus active owner/generation/exact expiry, rejects expired or over-horizon state, computes `max(current_expiry, now + lease_secs)` without changing generation, persists one receipt and one ordered renewal event, replays identical live receipts exactly once, rejects renewal-ID input conflicts, and records sanitized known-job rejection evidence without extending the lease | Evidence: fixed-time store tests cover the full fence tuple, every duration boundary, unchanged-short-target success, receipt expiry/cleanup, per-job isolation, and rollback atomicity | Verify: `cargo test -p harness-workflow runtime_store_remote_host_lease`
- [ ] `SP1602-T005` Owner: `completion-fencing` | Dependencies: `SP1602-T003`, `SP1602-T004` | Covers: B-003, B-004, B-008, B-014, B-015 | Done when: completion accepts additive optional `lease_generation`; upgraded clients are fenced by host, exact expiry, and generation while requests without generation preserve the existing V1 contract; stale, expired, reclaimed, revoked, or wrong-generation completion cannot mutate job or workflow state | Evidence: compatibility and fixed-time race tests prove a legacy valid completion still succeeds and old-generation or ownership-lost completion is rejected | Verify: `cargo test -p harness-workflow runtime_store_remote_host_lease` and `cargo test -p harness-server runtime_hosts_workflow_api`
- [ ] `SP1602-T006` Owner: `renewal-http-api` | Dependencies: `SP1602-T001`, `SP1602-T004`, `SP1602-T005` | Covers: B-002, B-003, B-005, B-007, B-008, B-012, B-013, B-014 | Done when: the authenticated renew route is registered; only registered active hosts reach the store transaction; store results map to documented `200` replay/new-success, `400`, `404`, `409 lease_lost` with `must_stop: true`, or `503`; malformed/unknown requests do not fabricate job audit evidence and ownership-loss responses disclose no winning host or lease data | Evidence: router/handler tests cover authentication, every response class, replay payload stability, heartbeat separation, one-job renewal isolation, and suppression of stale completion | Verify: `cargo test -p harness-server runtime_job_lease_renewal` and `cargo test -p harness-server runtime_hosts_workflow_api`
- [ ] `SP1602-T007` Owner: `host-draining-revocation` | Dependencies: `SP1602-T003`, `SP1602-T004`, `SP1602-T006` | Covers: B-003, B-009, B-010, B-011, B-015 | Done when: the additive `active | draining` host lifecycle defaults legacy records to active; deregistration durably enters draining before cleanup, blocks claims and renewals, row-locks and revokes every owned running RemoteHost job to pending with one event per job, clears live receipts, releases legacy claims, and removes the host only after cleanup is complete; retries resume partial cleanup without duplicate effects | Evidence: deterministic failpoints after draining and after revocation prove partial failures remain visible/retryable, while renew/complete/deregister races have one durable winner | Verify: `cargo test -p harness-server runtime_host_deregister` and `cargo test -p harness-workflow runtime_store_remote_host_lease`
- [ ] `SP1602-T008` Owner: `remote-protocol-docs` | Dependencies: `SP1602-T006`, `SP1602-T007` | Covers: B-005, B-008, B-009, B-010, B-012, B-013, B-014 | Done when: `docs/runtime-host-model-spec.md` documents the lease range and target-TTL calculation, one-third-TTL cadence, one in-flight renewal per job, same-ID retry after ambiguous transport, mandatory stop and completion suppression before last-confirmed expiry, heartbeat/job independence, draining deregistration, sanitized errors, additive compatibility, and rollback behavior | Evidence: documentation review maps each protocol obligation to the final wire fields and stable error classes | Verify: `rg -n "renew|renewal_id|lease_generation|must_stop|draining|heartbeat" docs/runtime-host-model-spec.md`

## Verification Tasks

- [ ] `SP1602-T009` Owner: `deterministic-db-verification` | Dependencies: `SP1602-T004`, `SP1602-T005`, `SP1602-T006`, `SP1602-T007` | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011, B-012, B-013, B-014, B-015 | Done when: PostgreSQL-backed tests use fixed timestamps plus barriers/failpoints rather than sleeps and cover every product-to-test row, including renew/reclaim, renew/complete, and renew/deregister single-winner races; the executed test output proves a configured `HARNESS_DATABASE_URL` was used and no DB suite silently skipped | Evidence: PR verification records the database URL configuration method, all focused command results, and the exact B-001 through B-015 coverage set | Verify: `cargo test -p harness-workflow runtime_store_remote_host_lease`, `cargo test -p harness-server runtime_job_lease_renewal`, `cargo test -p harness-server runtime_host_deregister`, and `cargo test -p harness-server runtime_hosts_workflow_api`
- [ ] `SP1602-T010` Owner: `integration-gates` | Dependencies: `SP1602-T008`, `SP1602-T009` | Covers: none — repository-wide build, formatting, lint, and SpecRail checks validate integration quality rather than a distinct product invariant | Done when: all focused suites, cross-crate compilation, formatting, workspace clippy, and both SpecRail checks pass from the final implementation head; any environment-dependent evidence is explicitly identified instead of inferred | Evidence: fresh command output is attached to the implementation PR or its verification artifact | Verify: `cargo check --workspace --all-targets`, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1602`, and `python3 checks/check_workflow.py --repo .`

## Parallelization

- `SP1602-T001` and `SP1602-T002` may run in parallel because their writable ownership is disjoint: server lease/API contract files versus workflow migration/model files.
- `SP1602-T003`, `SP1602-T004`, and `SP1602-T005` are serial in that order. They share the workflow store lease/completion transaction surface, and no parallel lane may write `crates/harness-workflow/src/runtime/store.rs` or its extracted lease modules at the same time.
- `SP1602-T006` starts after the renewal and completion store contracts stabilize. Its writable ownership is limited to the runtime-host handler/router and server API tests.
- `SP1602-T007` starts after `SP1602-T006`; deregistration crosses the host registry, runtime-host handler, and workflow store, so it must not overlap any lane writing those files.
- `SP1602-T008` may run in parallel with `SP1602-T009` after the final wire/lifecycle contract is fixed because documentation and deterministic test files are disjoint. If tests require production-file edits, ownership returns to the corresponding implementation owner before changes proceed.
- `SP1602-T010` is the final serial integration gate. No task may claim completion from skipped PostgreSQL tests or stale command output.

## Verification

- `cargo test -p harness-server runtime_job_lease_duration`
- `cargo test -p harness-workflow runtime_model`
- `cargo test -p harness-workflow runtime_store_remote_host_lease`
- `cargo test -p harness-server runtime_job_lease_renewal`
- `cargo test -p harness-server runtime_host_deregister`
- `cargo test -p harness-server runtime_hosts_workflow_api`
- `cargo check --workspace --all-targets`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1602`
- `python3 checks/check_workflow.py --repo .`

## Handoff Notes

- Product invariant set: `B-001` through `B-015`. Task coverage union: `B-001` through `B-015`; no invariant is omitted.
- The durable ordering boundary is non-negotiable: claim/reclaim, renew, complete/cancel, and deregistration revocation must use the same row-locked lease order, and lease mutation plus success/revocation evidence must commit together.
- Preserve the exact renewal semantics from the tech spec: target TTL from server time, generation unchanged during renewal, identical live receipt replay returns the original expiry, and expiry/ownership loss takes precedence over receipt replay.
- Host registry persistence and PostgreSQL revocation cannot be one transaction. Persisted `draining` is the recovery contract; never report deregistration success while the host owns a workflow runtime-job lease.
- Existing clients that omit renewal and completion generation keep their documented finite-lease behavior. New fields are additive; do not fabricate renewal receipts for legacy persisted jobs.
- Security review must confirm the existing API-auth middleware still covers the new route and that conflict/audit payloads never expose another host's identity, generation, or expiry.
- Rollback first stops clients from renewing, then disables the route. Before removing draining/revocation behavior, verify no host remains draining and no runtime job depends on a renewed expiry.
