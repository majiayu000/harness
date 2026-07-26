# Task Plan

## Linked Issue

GH-1730

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1730-T1` Owner: implementation agent | Dependencies: none | Done when: the module boundary and every closed v0.1 vocabulary enforce their exact wire and compatibility rules | Verify: focused schema, selection, and trust tests below | Covers: B-001, B-002, B-004, B-005, B-007, B-008, B-009
- [ ] `SP1730-T2` Owner: implementation agent | Dependencies: SP1730-T1 | Done when: source identity, locator, integrity, and public parsing fail closed with typed errors | Verify: focused identity, digest, and optional-fact tests below | Covers: B-003, B-006, B-010
- [ ] `SP1730-T3` Owner: implementation agent | Dependencies: SP1730-T1 and SP1730-T2 | Done when: component serialization and capability ordering are deterministic and round-trip safely | Verify: focused canonical wire-order tests below | Covers: B-011
- [ ] `SP1730-T4` Owner: implementation agent | Dependencies: SP1730-T1 through SP1730-T3 | Done when: exhaustive positive and negative fixtures prove every v0.1 invariant | Verify: focused and full harness-core suites below | Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011
- [ ] `SP1730-T5` Owner: verification owner | Dependencies: SP1730-T1 through SP1730-T4 | Done when: the additive scope and repository handoff gates are green on the exact implementation head | Verify: formatting, package, clippy, SpecRail, and diff-scope commands below | Covers: B-012

### SP1730-T1 — Add the closed component vocabulary and module boundary

- Owner: implementation agent
- Files: `crates/harness-core/src/lib.rs`,
  `crates/harness-core/src/stack/mod.rs`
- Dependencies: none
- Covers: B-001, B-002, B-004, B-005, B-007, B-008, B-009
- Done when:
  - `harness_core::stack` exposes the schema version and every closed enum
    named by the product spec.
  - Serde uses the exact snake_case wire spellings and rejects unknown enum
    values without aliases or catch-all variants.
  - Observation, selection, trust, and freshness combinations fail closed
    according to the product invariants.
- Verify:
  - `cargo check -p harness-core --all-targets`
  - `cargo test -p harness-core stack::tests::component_kind_wire_vocabulary_is_closed`
  - `cargo test -p harness-core stack::tests::selection_state_requires_supporting_observation`
  - `cargo test -p harness-core stack::tests::trust_cannot_exceed_observation_source`

### SP1730-T2 — Implement canonical source identity, integrity, and typed parsing

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/mod.rs`
- Dependencies: SP1730-T1
- Covers: B-003, B-006, B-010
- Done when:
  - Component IDs are derived only from the validated source scope, component
    kind, and source locator.
  - Repository locators reject absolute, drive-prefixed, backslash, empty,
    dot, and traversal segments without filesystem access.
  - Integrity accepts only non-zero lowercase SHA-256 values; omission remains
    distinct from explicit JSON `null`.
  - The public JSON entry point distinguishes syntax/shape failures from typed
    invariant failures without an untyped public escape hatch.
- Verify:
  - `cargo test -p harness-core stack::tests::component_id_is_canonical_kind_source_derivation_and_locator_is_portable`
  - `cargo test -p harness-core stack::tests::sha256_digest_rejects_blank_malformed_and_mixed_case_values`
  - `cargo test -p harness-core stack::tests::missing_optional_facts_are_not_fabricated`

### SP1730-T3 — Canonicalize component serialization

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/mod.rs`
- Dependencies: SP1730-T1, SP1730-T2
- Covers: B-011
- Done when:
  - Every component serializes deterministically with the specified field
    names and nested source shape.
  - Capabilities are unique and serialize in lexicographic order by exact
    snake_case wire spelling.
  - Missing integrity is omitted, explicit `null` is rejected, and
    deserialize/serialize round trips preserve semantic values.
- Verify:
  - `cargo test -p harness-core stack::tests::all_component_values_round_trip_in_canonical_wire_order`
  - `cargo test -p harness-core stack::tests::schema_version_is_required_and_exact`

### SP1730-T4 — Add exhaustive positive and negative contract tests

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/tests.rs`
- Dependencies: SP1730-T1, SP1730-T2, SP1730-T3
- Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008,
  B-009, B-010, B-011
- Done when:
  - Table-driven tests enumerate every closed wire value and every legal or
    illegal observation/selection/trust cross-product.
  - Exact-shape fixtures prove canonical identity, capability ordering,
    optional-integrity behavior, and unknown-field rejection.
  - Negative fixtures separately prove JSON syntax/shape failures and typed
    domain validation failures.
- Verify:
  - `cargo test -p harness-core stack`
  - `cargo test -p harness-core`

### SP1730-T5 — Prove additive scope and complete the handoff

- Owner: verification owner
- Files: `crates/harness-core/src/lib.rs`,
  `crates/harness-core/src/stack/mod.rs`,
  `crates/harness-core/src/stack/tests.rs`
- Dependencies: SP1730-T1, SP1730-T2, SP1730-T3, SP1730-T4
- Covers: B-012
- Done when:
  - The implementation changes only the three paths declared by the technical
    spec and adds no dependency, persistence migration, producer, or consumer.
  - Existing `Capability`, `ContextItem`, `CapabilityToken`, and `RuntimeKind`
    behavior remains unchanged.
  - The implementation PR uses `Fixes #1730`; this spec PR remains
    non-closing.
- Verify:
  - `git diff --name-only origin/main...HEAD`
  - `cargo fmt --all`
  - `cargo fmt --all -- --check`
  - `cargo check -p harness-core --all-targets`
  - `cargo test -p harness-core`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `python3 checks/check_workflow.py --repo .`
  - `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1730`

## Parallelization

The implementation is intentionally serial. SP1730-T1 through SP1730-T3 all
modify `stack/mod.rs`; SP1730-T4 tests that exact contract, and SP1730-T5 owns
shared verification. No two writable lanes may edit these paths concurrently.
A read-only reviewer lane may inspect the exact diff after SP1730-T5.

## Verification

- [ ] Product invariant set:
  `B-001` through `B-012`.
- [ ] Task coverage union:
  `B-001` through `B-012`.
- [ ] `python3 checks/check_workflow.py --repo .`
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1730`
- [ ] Implementation commands listed under SP1730-T5 pass on the exact PR
  head.

## Handoff Notes

- PR #1760 is the heavy, spec-only slice. It must merge without closing
  GH-1730.
- The implementation uses a separate original issue branch and one final
  implementation PR after this packet is approved and
  `ready_to_implement` is recorded.
- The planned-change manifest is exhaustive: no implementation path outside
  `crates/harness-core/src/lib.rs`, `crates/harness-core/src/stack/mod.rs`, and
  `crates/harness-core/src/stack/tests.rs` is authorized by this packet.
- ASC-002 and later issues consume this contract; they must not redefine its
  enums, stable component ID derivation, capability ordering, or parser error
  boundary.
