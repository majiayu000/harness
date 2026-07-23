# Task Plan

## Linked Issue

GH-1717

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1717-T1` Owner: protocol contract | Done when: protocol-owned DTOs preserve the complete legacy wire contract | Verify: focused protocol golden, defaults, and malformed-payload tests
- [ ] `SP1717-T2` Owner: protocol integration | Done when: the public method uses protocol DTOs and the normal composer dependency edge is absent | Verify: protocol check, parsing test, and dependency assertion
- [ ] `SP1717-T3` Owner: server boundary | Done when: exhaustive typed conversion preserves every field and ordering | Verify: focused conversion tests and server check
- [ ] `SP1717-T4` Owner: server integration tests | Done when: composer success, run-ID fallback, and error mapping remain unchanged | Verify: focused router tests
- [ ] `SP1717-T5` Owner: integration verifier | Done when: local, SpecRail, CI, review, and compatibility evidence is complete | Verify: final verification matrix below

### SP1717-T1 — Define protocol-owned context preview DTOs

- Owner: protocol contract
- Files: `crates/harness-protocol/src/context.rs`, `crates/harness-protocol/src/lib.rs`
- Dependencies: none
- Covers: B-002, B-003, B-004
- Done when:
  - Request, task-profile, item-ID, item, class, priority, degradation, and
    NAP-bearing DTOs reproduce the legacy wire contract.
  - Literal golden fixtures cover every item class, priority, degradation
    variant, and NAP status.
  - Optional and defaulted fields preserve their legacy serialization and
    deserialization behavior.
  - Missing required fields, unknown enums, and malformed degradation payloads
    fail deserialization.
- Verify:
  - `cargo test -p harness-protocol context_preview_wire_json_matches_legacy_golden_fixtures --lib`
  - `cargo test -p harness-protocol context_preview_defaults_and_empty_collections_are_equivalent --lib`
  - `cargo test -p harness-protocol context_preview_rejects_malformed_payloads --lib`

### SP1717-T2 — Move the public method boundary and remove the dependency edge

- Owner: protocol integration
- Files: `crates/harness-protocol/src/methods.rs`,
  `crates/harness-protocol/Cargo.toml`, `Cargo.lock`
- Dependencies: SP1717-T1
- Covers: B-001, B-002, B-003, B-004, B-008
- Done when:
  - `Method::ContextPreview` uses protocol-owned DTOs while retaining its
    existing method and field names and `supplied_items` default.
  - The normal `harness-context` dependency is absent from the protocol
    manifest, lockfile dependency list, and normal dependency graph.
- Verify:
  - `cargo check -p harness-protocol --all-targets`
  - `cargo test -p harness-protocol context_rpc_preview_slash_method_deserializes --lib`
  - Run the dependency assertion in `tech.md`.

### SP1717-T3 — Add exhaustive server boundary conversion

- Owner: server boundary
- Files: `crates/harness-server/src/handlers/context.rs`
- Dependencies: SP1717-T1; integration verification requires SP1717-T2
- Covers: B-005, B-006, B-007
- Done when:
  - Private typed conversion functions explicitly map every field and
    exhaustively match every closed enum.
  - Conversion preserves values and vector order without fallback, filtering,
    sorting, or caller-visible mutation.
  - Existing run-ID fallback and composition execute only after conversion.
- Verify:
  - `cargo test -p harness-server context_preview_conversion_preserves_every_field --lib`
  - `cargo test -p harness-server context_preview_conversion_is_deterministic_and_order_preserving --lib`
  - `cargo check -p harness-server --all-targets`

### SP1717-T4 — Prove unchanged router and composer behavior

- Owner: server integration tests
- Files: `crates/harness-server/src/router/tests/observability.rs`
- Dependencies: SP1717-T2, SP1717-T3
- Covers: B-005, B-007
- Done when:
  - Supplied protocol items still reach rendered output and the preview
    manifest.
  - Omitted request `run_id` still uses the current run identity.
  - Composer failures retain the existing JSON-RPC error mapping.
- Verify:
  - `cargo test -p harness-server context_rpc_preview_with_supplied_items_returns_manifest --lib`
  - `cargo test -p harness-server context_preview_uses_current_run_id_when_request_omits_run_id --lib`
  - `cargo test -p harness-server context_preview_preserves_composer_error_mapping --lib`

## Parallelization

- After SP1717-T1, SP1717-T2 and SP1717-T3 may edit concurrently because their
  file ownership is disjoint; verify their combined state before SP1717-T4.
- SP1717-T4 waits for both integration surfaces.
- `Cargo.lock` belongs exclusively to SP1717-T2.

## Verification

### SP1717-T5 — Collect completion and compatibility evidence

- Owner: integration verifier
- Files: none; verification and PR handoff only
- Dependencies: SP1717-T1, SP1717-T2, SP1717-T3, SP1717-T4
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008
- Done when:
  - Every targeted test and dependency assertion passes from the implementation
    head.
  - Workflow validation, formatting, package checks, and pre-push clippy pass.
  - The implementation PR documents the Rust source-compatibility impact for
    direct `Method::ContextPreview` constructors.
- Verify:
  - `cargo fmt --all -- --check`
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1717`
  - `cargo clippy --workspace --all-targets -- -D warnings`

## Handoff Notes

- Preserve protocol DTO and composer-domain ownership separation.
- Do not add aliases, wildcard conversions, untyped bridges, response DTOs, or
  a shared-types crate.
- Keep the handler sequence: typed conversion, run-ID fallback, existing
  composer call, and existing response mapping.
- Use literal legacy golden JSON so protocol tests do not recreate the removed
  dependency.
