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
- `AgentStackComponentId`;
- `AgentStackComponentKind`;
- `AgentStackSourceScope`;
- `AgentStackSourceLocator`;
- `AgentStackUserGlobalRoot`;
- `AgentStackObservationClass`;
- `AgentStackSelectionState`;
- `AgentStackCapability`;
- `AgentStackTrustLevel`;
- `AgentStackFreshness`;
- `Sha256Digest`;
- `AgentStackSource`;
- `AgentStackComponent`;
- `AgentStackComponentError`;
- `AgentStackComponentParseError`.

Every enum derives serde with `rename_all = "snake_case"` and has no catch-all
variant. `AgentStackComponentId`, `AgentStackSourceLocator`, and `Sha256Digest`
are public validated newtypes with private inner strings. The component and
nested source structs use `deny_unknown_fields`; invariant-bearing fields are
private and exposed through read-only accessors. `AgentStackComponentError`
owns typed construction and validation failures.
`AgentStackComponentParseError` is the public syntax-versus-validation wrapper.
Private `VersionEnvelope` and v0.1 wire structs are implementation details and
never appear in the public API.

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

`AgentStackSourceScope` contains explicit `repository`, `user_global`, `admin`,
`system`, `runtime`, and `runner` values.

Repository locators use `/`-separated portable paths relative to the repository
root. Admin locators use the same path grammar relative to `/etc/harness`.
Neither scope serializes its absolute root. User-global locators use
`<root_namespace>/<portable_relative_path>`, with this closed root namespace:

| Root namespace | Exact logical root |
| --- | --- |
| `home_harness` | `$HOME/.harness` |
| `xdg_config_harness` | `$XDG_CONFIG_HOME/harness` when XDG_CONFIG_HOME is absolute; otherwise `$HOME/.config/harness` |
| `platform_config_harness` | macOS `$HOME/Library/Application Support/harness` or Windows `%APPDATA%/harness` |
| `configured_user` | A user-owned configured root with a persisted snake_case configuration key as the first relative segment |

The model exposes a pure user-global root selector. Producers pass expanded,
lexically normalized absolute candidate roots plus the unnormalized source
path; the helper performs no environment reads or filesystem I/O. v0.1 accepts
zero or one `configured_user` candidate. Supplying more than one returns
`AgentStackComponentError::AmbiguousConfiguredUserRoot` before path matching,
including equal or nested configured roots. If a source path matches more than
one remaining root, the selector chooses the first match in this exact
precedence: `home_harness`, `xdg_config_harness`,
`platform_config_harness`, then the single `configured_user`. Equal default
roots collapse to the higher-precedence namespace, so XDG wins over the
platform root in the macOS/Windows collision cases and matches `config::dirs`
discovery order. A collision exists only when normalized root segment sequences
are lexically equal. The root-match key canonicalizes only Windows disk-prefix
letters to uppercase; every other path component remains case-sensitive. Thus
`C:\root` and `c:\root` are equivalent roots, while `C:\Root` and `C:\root`
remain distinct logical sources in v0.1. The selector does not query
filesystem-specific case-sensitivity. Producers must pass consistently
expanded candidate roots. Focused tests separately prove drive-letter
equivalence and directory-segment case distinction through the same
platform-neutral root-match helper used by the selector. Containment is
segment-aware, not string-prefix based. Root comparison does not resolve
symlinks; differently configured symlink roots are distinct logical sources in
v0.1. An absent or relative environment root is a producer discovery error, not
a wire-parse error.

System locators use `builtin/<namespace>/<stable_path>` for embedded
components. Runtime and runner locators use
`<namespace>/<stable_path>`. `namespace` uses lowercase snake_case ASCII
(`[a-z0-9]+(?:_[a-z0-9]+)*`). `stable_path` contains one or more
`/`-separated portable identity segments; each segment starts with ASCII
alphanumeric and continues with ASCII alphanumeric, `_`, `-`, or `.` while
preserving case exactly. This preserves identities such as `exec-plan`,
`golden-principles.md`, `codex-default`, `getUser`, and `DATA_EXPORT_v2`
without aliases; `getUser` and `getuser` are distinct identities. Validation
rejects UUID-shaped segments case-insensitively, including canonical
hyphenated UUIDs and 32-hex compact UUIDs, whitespace-bearing display labels,
empty segments, `.`, and `..`. It also rejects the reserved missing-evidence
spellings `unknown`, `unknown-component`, `unknown_component`, `none`, `null`,
and `missing` case-insensitively and independently in every logical-path
segment before component-ID derivation.

All filesystem-derived locators require lossless UTF-8. The path adapter
decomposes the root and source with `Path::components()` and performs an
explicit segment-aware prefix comparison with the root-match key above, without
filesystem canonicalization. A Windows `Disk` or `VerbatimDisk` prefix is
represented by an internal typed prefix whose ASCII drive letter is uppercase;
other prefixes and roots are rejected for portable locators. After the matched
root, the adapter rejects `Prefix`, `RootDir`, and `ParentDir`, and discards
redundant `CurDir` components, so `root/a/./b` derives the same locator as
`root/a/b`, while `root/a/../b` fails. Every accepted `OsStr` segment uses
`to_str()` independently; failure returns
`AgentStackComponentError::NonUtf8SourceLocator`.

The path adapter delegates output to a platform-neutral
`encode_portable_segments` helper that accepts explicit UTF-8 segments,
validates each segment, and joins them with literal `/`. Contract tests exercise
this helper on every CI platform without claiming to execute Windows-native
`Path` semantics on Ubuntu. The wire parser separately rejects leading `/`,
backslashes, Windows drive prefixes such as `C:`, NUL bytes, empty segments,
`.`, and `..`. `to_string_lossy()` and filesystem I/O are prohibited.

The wire validator deliberately does not claim to prove how an otherwise-valid
stable path was obtained. Producers have the separate contract to use persisted
configuration identity rather than display names, process IDs, timestamps, or
other per-scan data. ASC-002 owns repository inventory mapping. ASC-003 owns
runtime prompt-provenance mappings in
`crates/harness-server/src/workflow_runtime_worker/prompt_packet/`; it preserves
the source scope and locator for repository/user sources observed at runtime and
uses `runtime` scope only for runtime-owned generated/configuration components.
ASC-004 owns runtime and MCP fingerprint mappings in `harness-agents` and
`harness-core`; it chooses scope from actual component ownership and uses
`runner_observed` only as observation class when a runner performs the probe.
Their producer tests must prove stable-path provenance and stable component ID
across stronger observations. ASC-005 compares snapshots and reports an
otherwise-unexplained path change as an identity discontinuity.

### Source Mapping Contract Examples

| Source class | v0.1 contract example |
| --- | --- |
| Repository files | `repository` plus repository-relative portable path |
| `$HOME/.harness` instructions, skills, and rules | `user_global` plus `home_harness/<relative_path>` |
| XDG/default user config | `user_global` plus `xdg_config_harness/<relative_path>` |
| macOS/Windows platform config | `user_global` plus `platform_config_harness/<relative_path>` after precedence collapse |
| Typed user-owned persist root | `user_global` plus `configured_user/<stable_config_key>/<relative_path>` |
| `/etc/harness` skills and rules | `admin` plus `/etc/harness`-relative portable path |
| Embedded/built-in skills or rules | `system` plus `builtin/<namespace>/<stable_path>` |
| Runtime-owned prompt settings or synthesized documents | `runtime`; producer implementation belongs to ASC-003 |
| Repository/user component loaded by runtime | Original source scope and locator, plus `runtime_observed` observation |
| Runner-owned executable/probe component | `runner`; producer implementation belongs to ASC-004 |
| Repository/user runtime or MCP component probed by runner | Original source scope and locator, plus `runner_observed` observation |

`HARNESS_DATA_DIR` is persistence configuration, not a component source by
itself. Arbitrary `SkillStore` or `RuleEngine` discovery paths without typed
ownership and a persisted configuration key cannot emit v0.1 components; a
future producer must first classify them as repository, user, admin, or system
input. This fails closed instead of guessing from an absolute path. This issue
implements contract examples only, not integrations with those stores.
ASC-002, ASC-003, and ASC-004 own the repository, runtime-prompt, and
runtime/MCP producers respectively. User/admin/system inventory outside those
issues remains intentionally unallocated and requires a new linked spec before
integration.

`AgentStackComponentId::from_source(kind, source)` is the only component-ID
derivation. Its exact wire spelling is
`<source_scope>:<component_kind>:<source_locator>` using the closed snake_case
enum spellings and the validated locator. Construction and parsing recompute
that value and reject any supplied ID that differs. This guarantees that the
ID agrees with the validated source fields; it does not independently attest
the producer's cross-scan stable-key provenance.

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
2. valid scope-specific source locator, including root namespace, lossless
   UTF-8, NUL, reserved-sentinel, UUID-shape, and logical-path grammar;
3. component ID exactly matching the canonical kind/source derivation;
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
`observed` requires runtime or runner observation. The exact trust matrix is:

| Observation class | Allowed trust levels |
| --- | --- |
| `repository_observed` | `self_declared`, `repository_observed` |
| `runtime_observed` | `self_declared`, `repository_observed`, `runtime_observed` |
| `runner_observed` | `self_declared`, `repository_observed`, `runtime_observed`, `runner_observed` |

`AgentStackComponent` implements deterministic `Serialize` but does not expose
raw untyped `Deserialize`. Public `AgentStackComponent::from_json(&str)` first
parses a private minimal version envelope that reads only `schema_version` and
does not reject additional fields. A missing, blank, or non-v0.1 version
returns the typed unsupported-version validation error before shape decoding.
Only an exact v0.1 envelope is then parsed into the strict private v0.1 wire
representation with `deny_unknown_fields` and converted through `TryFrom`.
Malformed JSON or an invalid v0.1 shape returns
`AgentStackComponentParseError::Syntax`; domain-invariant failures return
`AgentStackComponentParseError::Validation(AgentStackComponentError)`. Thus a
future v0.2 object with new fields is classified by version rather than being
misreported as a v0.1 unknown-field syntax failure. Normal Rust construction
uses `AgentStackComponent::new(...)` and validated setters/builders; struct
fields that participate in invariants are not publicly mutable.

Wire parsing is platform- and environment-independent. It validates the closed
scope/root vocabulary and locator syntax but never reads `HOME`,
`XDG_CONFIG_HOME`, `APPDATA`, the current platform, or the filesystem. Producer
constructors own environment applicability, root precedence, path conversion,
and typed discovery errors. Therefore evidence produced on macOS parses
identically on Linux.

Source scope and observation class are orthogonal. Parsing or constructing a
stronger observation never rewrites `source` or `component_id`. A repository
skill discovered by ASC-002 and later loaded by ASC-003 therefore retains the
same `repository:skill:<locator>` identity while its observation class and
selection state advance.

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
| B-001 | schema constant and version-envelope-first parsing | `cargo test -p harness-core stack::tests::schema_version_is_required_and_exact`; `cargo test -p harness-core stack::tests::unsupported_version_precedes_strict_v01_shape_validation` |
| B-002 | `AgentStackComponentKind` | `cargo test -p harness-core stack::tests::component_kind_wire_vocabulary_is_closed` |
| B-003 | source-mapping contract examples, canonical root selection, lossless locator encoding, and component-ID derivation | `cargo test -p harness-core stack::tests::source_mapping_contract_examples_are_canonical`; `cargo test -p harness-core stack::tests::component_identity_is_stable_across_observation_classes`; `cargo test -p harness-core stack::tests::user_global_root_selection_collapses_overlaps_by_precedence`; `cargo test -p harness-core stack::tests::multiple_configured_user_roots_fail_as_ambiguous`; `cargo test -p harness-core stack::tests::wire_parser_is_environment_independent`; `cargo test -p harness-core stack::tests::path_locator_rejects_non_utf8_without_lossy_conversion`; `cargo test -p harness-core stack::tests::portable_segment_encoder_uses_forward_slashes`; `cargo test -p harness-core stack::tests::path_adapter_canonicalizes_curdir_and_rejects_parentdir`; `cargo test -p harness-core stack::tests::windows_drive_letter_casing_is_canonical_equivalent`; `cargo test -p harness-core stack::tests::windows_directory_segment_casing_remains_distinct`; `cargo test -p harness-core stack::tests::logical_path_grammar_covers_system_runtime_and_runner`; `cargo test -p harness-core stack::tests::logical_path_preserves_case_distinct_tool_names` |
| B-004 | `AgentStackObservationClass` | `cargo test -p harness-core stack::tests::observation_class_round_trips_without_implied_trust` |
| B-005 | observation/selection validation matrix | `cargo test -p harness-core stack::tests::selection_state_requires_supporting_observation` |
| B-006 | `Sha256Digest` newtype | `cargo test -p harness-core stack::tests::sha256_digest_rejects_blank_malformed_and_mixed_case_values` |
| B-007 | `AgentStackCapability` | `cargo test -p harness-core stack::tests::capability_wire_vocabulary_is_closed` |
| B-008 | observation/trust validation matrix | `cargo test -p harness-core stack::tests::trust_cannot_exceed_observation_source` |
| B-009 | `AgentStackFreshness` | `cargo test -p harness-core stack::tests::missing_freshness_is_explicitly_unknown` |
| B-010 | constructors, reserved locator rejection, and optional-field validation | `cargo test -p harness-core stack::tests::source_locator_rejects_reserved_sentinels`; `cargo test -p harness-core stack::tests::missing_optional_facts_are_not_fabricated` |
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
- Logic: overlapping user roots could fork stable IDs; the pure selector and
  synthetic cross-platform collision fixtures enforce one precedence.
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
      NUL locator, reserved-sentinel locator, runtime/runner UUID and
      display-label locator, duplicate and canonically ordered capability, and
      explicit-null integrity tests.
- [ ] Prove user-global producers using the same root namespace derive the same
      locator, reject unknown root namespaces, and collapse equal or overlapping
      XDG/platform roots by the fixed precedence.
- [ ] Prove multiple equal or nested configured-user roots fail as ambiguous.
- [ ] Prove system, runtime, and runner logical-path grammar in one
      table-driven suite, including system's fixed `builtin/` prefix and
      per-segment UUID, sentinel, and display-label rejection.
- [ ] Cover every row in the source-mapping contract-example table and prove
      untyped custom discovery roots fail closed without claiming integration
      coverage for producers outside this issue.
- [ ] Prove the wire parser is independent of platform/environment and
      filesystem path constructors reject non-UTF-8 without lossy conversion.
- [ ] Prove the path adapter discards `CurDir`, rejects `ParentDir`, and cannot
      escape the matched root.
- [ ] Prove the platform-neutral explicit-segment encoder uses `/`, rejects
      explicit `.` and `..` segments, and preserves existing
      hyphenated/dotted and case-distinct stable identities.
- [ ] Prove the platform-neutral root-match helper makes Windows drive-letter
      casing canonical-equivalent while preserving directory-segment casing.
- [ ] Prove stronger runtime/runner observation preserves the original source
      scope, locator, and component ID.
- [ ] Prove a future-version fixture with v0.1-unknown fields returns the typed
      unsupported-version error before strict v0.1 shape validation.
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
