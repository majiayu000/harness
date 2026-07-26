# Tech Spec

## Linked Issue

GH-1730

## Product Spec

See `specs/GH1730/product.md`.

<!-- specrail-planned-changes
{"issue":1730,"complete":true,"paths":["crates/harness-core/src/lib.rs","crates/harness-core/src/stack/mod.rs","crates/harness-core/src/stack/tests.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012"]}
-->

## Current System

- `crates/harness-core/src/lib.rs:1-29` exposes existing core modules but has no
  Agent Stack model.
- `crates/harness-core/src/types.rs:584-606` defines the existing coarse
  `Capability` and prompt-facing `ContextItem` types. They do not represent
  component provenance, integrity, selection state, or evidence trust.
- `crates/harness-core/src/agent.rs:26-70` defines `AgentRequest`, including
  context, tool allowlists, sandbox hints, environment variables, and an
  optional capability token. These are execution inputs and remain unchanged.
- `crates/harness-core/src/capability.rs:11-49` defines the scoped
  `CapabilityToken` enforcement object. The new model must not replace or
  weaken it.
- `crates/harness-skills/src/store.rs:22-45` already stores a skill
  `content_hash`, freshness inputs, and governance state, but the skill crate
  depends on core concepts and is not an appropriate home for the shared stack
  contract.
- `crates/harness-core/Cargo.toml:13-25` already provides serde, serde_json, and
  sha2 through workspace dependencies; this issue needs no new dependency.

## Proposed Design

### Module Boundary

Add `harness_core::stack` as the sole owner of the v0.1 component schema. Use a
directory module so ASC-002 and later stack features can add focused modules
without growing one file beyond repository limits.

`lib.rs` exports `pub mod stack;`. No existing re-export or existing public type
changes.

### Typed Component Contract

`stack/mod.rs` defines:

- `AGENT_STACK_COMPONENT_SCHEMA_VERSION`;
- `AgentStackComponentKind`;
- `AgentStackSourceScope`;
- `AgentStackObservationClass`;
- `AgentStackSelectionState`;
- `AgentStackCapability`;
- `AgentStackTrustLevel`;
- `AgentStackFreshness`;
- `AgentStackSource`;
- `AgentStackComponent`;
- `AgentStackComponentError`.

Every enum derives serde with `rename_all = "snake_case"` and has no catch-all
variant. The component and nested source structs use `deny_unknown_fields`.
Required strings remain `String` in the serialized shape but construction and
`validate()` reject blank values.

The exact v0.1 wire object has these fields in this struct declaration order:

```json
{
  "schema_version": "agent-stack-component/v0.1",
  "component_id": "repository:skill:skills/example/SKILL.md",
  "kind": "skill",
  "source": {
    "scope": "repository",
    "locator": "skills/example/SKILL.md"
  },
  "observation_class": "repository_observed",
  "selection_state": "discovered",
  "integrity": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "capabilities": [],
  "trust_level": "repository_observed",
  "freshness": "unknown"
}
```

All fields except `integrity` are required. `source` contains exactly `scope`
and `locator`; neither the top-level object nor `source` accepts unknown
fields. `integrity` uses a custom `deserialize_with` function plus a missing
field default, and `#[serde(skip_serializing_if = "Option::is_none")]` for
output. A missing field becomes `None`, serialization omits `None`, and an
explicit JSON `null` is rejected rather than being accepted as absence.
Deserialization rejects a missing required field rather than inventing a
default.

`AgentStackSourceScope` contains explicit `repository`, `user_global`,
`runtime`, and `runner` values. Repository locators use `/`-separated
repository-relative strings. Platform-neutral validation rejects leading `/`,
backslashes, Windows drive prefixes such as `C:`, and empty, `.`, or `..`
segments before accepting the locator; it does not use platform-specific
`std::path::Component` semantics or access the filesystem. Other scopes
require a non-empty opaque locator and are never rewritten into repository
paths.

`AgentStackComponentId::from_source(kind, source)` is the only component-ID
derivation. Its exact wire spelling is
`<source_scope>:<component_kind>:<source_locator>` using the closed snake_case
enum spellings and the validated locator. Construction and parsing recompute
that value and reject any supplied ID that differs, so UUIDs, mutable labels,
and producer-specific aliases cannot create parallel identities for the same
source.

Integrity is represented by `Option<Sha256Digest>`, where `Sha256Digest` is a
validated newtype around the 64-character lowercase hexadecimal wire value.
The all-zero value is rejected as the missing-integrity sentinel prohibited by
B-010. This issue validates supplied digests but does not hash content. Digest
calculation belongs to inventory and snapshot producers.

Capabilities use the sensitivity vocabulary required by B-007. They are kept
distinct from `types::Capability`, which describes broad adapter support, and
from `CapabilityToken`, which grants scoped write authority. ASC-008 will wrap
these identifiers in declared/granted/observed evidence without changing them.
Canonical serialization sorts capabilities lexicographically by their exact
snake_case wire spelling:
`destructive`, `file_write`, `network`, `privileged`, `production_write`,
`secret_read`, `shell`.

### Validation Invariants

`AgentStackComponent::validate()` returns the first typed invariant violation:

1. exact schema version;
2. component ID exactly matching the canonical kind/source derivation;
3. valid source scope and locator;
4. valid optional digest;
5. observation/selection compatibility;
6. observation/trust compatibility;
7. no duplicate capability values.

Capabilities remain a sequence in the wire representation so deserialization
can reject duplicates instead of silently deduplicating untrusted evidence.
Validation sorts by the exact wire spelling above only after uniqueness has
been proven.

Repository observations permit only `discovered`, `eligible`, or `selected`.
Runtime and runner observations permit every selection state. A
repository-level `selected` claim does not prove runtime loading or use.
`observed` requires runtime or runner observation. Trust may be equal to or
weaker than the observation source but never stronger. `self_declared` is
accepted for every observation source.

`AgentStackComponent` implements deterministic `Serialize` but does not expose
raw untyped `Deserialize`. Public `AgentStackComponent::from_json(&str)` first
parses a private wire representation and then applies `TryFrom`, returning
`AgentStackComponentParseError::Syntax` for JSON syntax/shape failures or
`AgentStackComponentParseError::Validation(AgentStackComponentError)` for
domain-invariant failures. This preserves the typed validation cause instead
of flattening it into `serde_json::Error`. Normal Rust construction uses
`AgentStackComponent::new(...)` and validated setters/builders; struct fields
that participate in invariants are not publicly mutable.

### Test Layout

Keep focused unit tests in `stack/tests.rs` via `#[cfg(test)] mod tests;`.
Table-driven tests enumerate every enum wire value and legal
observation/selection/trust combination. Exact-shape fixtures verify required
field names, nested source fields, canonical component identity, declaration-
order serialization, exact capability ordering, omission of absent integrity,
rejection of explicit `null`, and rejection of unknown fields. Negative JSON
fixtures remain schema-shaped so they test typed invariant rejection separately
from malformed-JSON syntax failures.

## Data Flow

Producer input → typed component constructor or typed JSON parser → ordered
invariant validation and canonicalization → immutable `AgentStackComponent` →
deterministic serde output.

Constructor failure returns `AgentStackComponentError`; JSON input failure
returns `AgentStackComponentParseError` while retaining any nested validation
cause. No default component, alias, warning-only fallback, filesystem mutation,
or persistence occurs.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | schema constant and deserialization validation | `cargo test -p harness-core stack::tests::schema_version_is_required_and_exact` |
| B-002 | `AgentStackComponentKind` | `cargo test -p harness-core stack::tests::component_kind_wire_vocabulary_is_closed` |
| B-003 | canonical component ID plus source scope and locator validation | `cargo test -p harness-core stack::tests::component_id_is_canonical_kind_source_derivation_and_locator_is_portable` |
| B-004 | `AgentStackObservationClass` | `cargo test -p harness-core stack::tests::observation_class_round_trips_without_implied_trust` |
| B-005 | observation/selection validation matrix | `cargo test -p harness-core stack::tests::selection_state_requires_supporting_observation` |
| B-006 | `Sha256Digest` newtype | `cargo test -p harness-core stack::tests::sha256_digest_rejects_blank_malformed_and_mixed_case_values` |
| B-007 | `AgentStackCapability` | `cargo test -p harness-core stack::tests::capability_wire_vocabulary_is_closed` |
| B-008 | observation/trust validation matrix | `cargo test -p harness-core stack::tests::trust_cannot_exceed_observation_source` |
| B-009 | `AgentStackFreshness` | `cargo test -p harness-core stack::tests::missing_freshness_is_explicitly_unknown` |
| B-010 | constructors and optional-field validation | `cargo test -p harness-core stack::tests::missing_optional_facts_are_not_fabricated` |
| B-011 | serde attributes, canonical capability order, and round-trip table | `cargo test -p harness-core stack::tests::all_component_values_round_trip_in_canonical_wire_order` |
| B-012 | manifest scope and existing core tests | `git diff --name-only origin/main...HEAD`; `cargo test -p harness-core` |

## Alternatives Considered

- Add fields to `ContextItem`: rejected because prompt content and stack
  evidence have different lifecycles, trust, and redaction requirements.
- Reuse `types::Capability`: rejected because its broad read/write/execute
  vocabulary cannot describe sensitive authority changes.
- Put the model in `harness-workflow`: rejected because CLI inventory and agent
  adapters also need the contract without depending on workflow runtime.
- Use `serde_json::Value` for forward compatibility: rejected because unknown
  fields and variants would silently bypass the v0.1 contract.
- Compute stable stack IDs now: rejected because canonical aggregate identity
  is ASC-005 and requires inventory/runtime producers not present here.

## Risks

- Security: source locators and validation errors must not contain file
  contents or secrets.
- Compatibility: closed enums intentionally reject future variants until the
  schema version changes.
- Logic: an incorrect observation/trust ordering could permit evidence
  escalation; exhaustive matrices cover the cross-product.
- Performance: validation is bounded by a small capability list and performs
  no I/O.
- Maintenance: later issues must extend this module rather than copy its
  vocabulary into parallel schemas.

## Test Plan

- [ ] Add round-trip tests for every enum and a representative component of
      every kind.
- [ ] Add exhaustive observation × selection and observation × trust tables.
- [ ] Add schema-valid negative fixtures for unknown fields and aliases.
- [ ] Add digest, canonical identity, drive-prefixed and traversal locator,
      duplicate and canonically ordered capability, and explicit-null integrity
      tests.
- [ ] Add separate typed assertions for JSON syntax/shape failures and domain
      validation failures.
- [ ] Run `cargo check -p harness-core --all-targets`.
- [ ] Run `cargo test -p harness-core stack`.
- [ ] Run `cargo test -p harness-core`.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Before push, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1730`.
- [ ] Confirm the implementation diff contains only the three paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. The module is additive and has no producer,
consumer, database migration, configuration change, or persisted data in this
issue. If dependent stack features have already landed, revert those consumers
first or in the same rollback so no unresolved module references remain.
