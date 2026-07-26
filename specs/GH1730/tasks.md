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
  - Public validated component ID, source locator, digest, freshness evidence,
    component, and parse error types expose read-only accessors without public
    invariant mutation.
  - Serde uses the exact snake_case wire spellings and rejects unknown enum
    values without aliases or catch-all variants.
  - The closed component-kind enum participates in component ID derivation, so
    explicit multi-role bindings produce distinct IDs without a core artifact
    classifier.
  - Observation, selection, trust, and freshness combinations fail closed
    according to the product invariants.
  - Observation-to-trust validation uses the complete matrix from B-008 rather
    than inferring an implementation-specific observer relationship.
  - The pure typed freshness-evidence helper uses
    expiry/current/cached/no-evidence precedence without reading the clock, and
    treats `observation_time == valid_until` as expired.
- Verify:
  - `cargo check -p harness-core --all-targets`
  - `cargo test -p harness-core stack::tests::component_kind_wire_vocabulary_is_closed`
  - `cargo test -p harness-core stack::tests::explicit_multi_role_bindings_have_distinct_component_ids`
  - `cargo test -p harness-core stack::tests::observation_class_round_trips_without_implied_trust`
  - `cargo test -p harness-core stack::tests::selection_state_requires_supporting_observation`
  - `cargo test -p harness-core stack::tests::trust_cannot_exceed_observation_source`
  - `cargo test -p harness-core stack::tests::capability_wire_vocabulary_is_closed`
  - `cargo test -p harness-core stack::tests::missing_freshness_is_explicitly_unknown`
  - `cargo test -p harness-core stack::tests::freshness_evidence_mapping_is_deterministic`
  - `cargo test -p harness-core stack::tests::freshness_deadline_is_expired_at_exact_boundary`
  - `cargo test -p harness-core stack::tests::explicit_expiry_precedes_current_and_cached_evidence`
  - `cargo test -p harness-core stack::tests::cached_without_current_observation_is_stale`

### SP1730-T2 — Implement canonical source identity, integrity, and typed parsing

- Owner: implementation agent
- Files: `crates/harness-core/src/stack/mod.rs`
- Dependencies: SP1730-T1
- Covers: B-003, B-006, B-010
- Done when:
  - Component IDs are derived only from the validated source scope, component
    kind, and source locator.
  - Repository locators are relative to the repository root; user-global
    locators use the closed canonical root namespace selected by the pure
    precedence helper; admin locators are relative to `/etc/harness`; system
    locators use the built-in logical namespace.
  - Equal or overlapping user-global roots collapse in the exact order
    `home_harness`, `xdg_config_harness`, `platform_config_harness`,
    `configured_user`; more than one configured-user candidate fails as
    ambiguous.
  - The configured-user key segment preserves valid lowercase snake_case and
    rejects aliases, case variants, UUIDs, reserved sentinels, and display
    labels before the remaining portable path is accepted.
  - A pure resolver uses absolute XDG when available, otherwise falls back from
    absent or relative XDG to absolute HOME, and returns a typed discovery error
    only when neither root is usable. Its lexically normalized output becomes
    the XDG candidate passed to the selector.
  - Filesystem-derived locators reject non-UTF-8, absolute, drive-prefixed,
    backslash, NUL, empty, and traversal inputs without filesystem access or
    lossy conversion. Redundant `.` canonicalizes away; `..` fails.
    Platform-neutral segment encoding joins valid components with `/`.
  - System, runtime, and runner locators enforce the stable logical-path wire
    grammar, preserve existing hyphenated/dotted stable identities, and reject
    reserved missing-evidence sentinels, UUIDs, and display-label shapes.
    Reserved sentinels are rejected independently in every segment. The parser
    does not claim to verify whether an otherwise-valid path came from persisted
    configuration.
  - Repository, user, admin, and system contract examples map deterministically;
    untyped custom discovery paths fail closed. Tests do not claim integration
    coverage for producers outside this issue.
  - Component source scope and locator remain unchanged when observation class
    strengthens from repository to runtime or runner observation.
  - Wire parsing is independent of platform, environment variables, and the
    filesystem; producer constructors own environment/root applicability.
  - Source locators are validated before their canonical component IDs are
    derived and compared.
  - Integrity accepts only non-zero lowercase SHA-256 values and hashes exact
    raw file or embedded payload bytes without implicit canonicalization.
    Standard empty-content SHA-256 remains distinct from omission and explicit
    JSON `null`; core parsing does not attest producer byte provenance.
  - The public JSON entry point distinguishes syntax/shape failures from typed
    invariant failures without an untyped public escape hatch, and checks a
    minimal version envelope before strict v0.1 shape decoding.
- Verify:
  - `cargo test -p harness-core stack::tests::source_mapping_contract_examples_are_canonical`
  - `cargo test -p harness-core stack::tests::component_identity_is_stable_across_observation_classes`
  - `cargo test -p harness-core stack::tests::user_global_root_selection_collapses_overlaps_by_precedence`
  - `cargo test -p harness-core stack::tests::multiple_configured_user_roots_fail_as_ambiguous`
  - `cargo test -p harness-core stack::tests::configured_user_key_uses_strict_snake_case`
  - `cargo test -p harness-core stack::tests::configured_user_key_rejects_display_uuid_and_reserved_segments`
  - `cargo test -p harness-core stack::tests::xdg_root_falls_back_to_absolute_home_when_xdg_is_missing_or_relative`
  - `cargo test -p harness-core stack::tests::xdg_root_fails_when_xdg_and_home_are_unusable`
  - `cargo test -p harness-core stack::tests::wire_parser_is_environment_independent`
  - `cargo test -p harness-core stack::tests::path_locator_rejects_non_utf8_without_lossy_conversion`
  - `cargo test -p harness-core stack::tests::portable_segment_encoder_uses_forward_slashes`
  - `cargo test -p harness-core stack::tests::path_adapter_canonicalizes_curdir_and_rejects_parentdir`
  - `cargo test -p harness-core stack::tests::windows_drive_letter_casing_is_canonical_equivalent`
  - `cargo test -p harness-core stack::tests::windows_directory_segment_casing_remains_distinct`
  - `cargo test -p harness-core stack::tests::logical_path_grammar_covers_system_runtime_and_runner`
  - `cargo test -p harness-core stack::tests::logical_path_preserves_case_distinct_tool_names`
  - `cargo test -p harness-core stack::tests::untyped_custom_discovery_source_fails_closed`
  - `cargo test -p harness-core stack::tests::source_locator_rejects_reserved_sentinels`
  - `cargo test -p harness-core stack::tests::runtime_locator_rejects_reserved_segments`
  - `cargo test -p harness-core stack::tests::portable_path_locator_rejects_nul`
  - `cargo test -p harness-core stack::tests::source_locator_validation_precedes_component_id_derivation`
  - `cargo test -p harness-core stack::tests::unsupported_version_precedes_strict_v01_shape_validation`
  - `cargo test -p harness-core stack::tests::sha256_digest_rejects_blank_malformed_and_mixed_case_values`
  - `cargo test -p harness-core stack::tests::sha256_digest_hashes_exact_source_bytes`
  - `cargo test -p harness-core stack::tests::sha256_digest_distinguishes_lf_crlf_bom_and_unicode_bytes`
  - `cargo test -p harness-core stack::tests::empty_content_digest_is_distinct_from_missing_integrity`
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
  - Kind fixtures cover the closed vocabulary and explicit multi-role identity
    separation without a test-only artifact classifier.
  - Source contract fixtures cover repository, user, admin, system, runtime,
    and runner examples plus overlapping-root, non-UTF-8, portable-segment,
    path-adapter, Windows root-casing, configured-user key, XDG fallback,
    logical-identity casing, and observation-stability cases.
  - Digest fixtures distinguish exact byte encodings and optional-field
    behavior; freshness fixtures exercise the production typed evidence helper
    and exact deadline boundary.
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

## Producer Handoff Requirements

These checks are mandatory in the linked producer issues, not executable
GH1730 `harness-core` tests:

- ASC-002, ASC-003, and ASC-004 must prove typed role-to-kind mapping and
  fail-closed handling for untyped or ambiguous discovery on their actual
  registration surfaces.
- Each producer must omit integrity when its source has no versioned canonical
  byte encoding and hash exact source bytes when one exists.
- ASC-002 must prove `harness_skills::FreshnessClass` does not determine
  `AgentStackFreshness`; this stays in `harness-skills` to avoid a crate
  dependency cycle.

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
