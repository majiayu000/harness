# Tech Spec

## Linked Issue

GH-1733

## Product Spec

See `specs/GH1733/product.md`.

<!-- specrail-planned-changes
{"issue":1733,"complete":true,"paths":["crates/harness-agents/src/lib.rs","crates/harness-agents/src/runtime_fingerprint.rs","crates/harness-agents/src/runtime_fingerprint/environment.rs","crates/harness-agents/src/runtime_fingerprint/executable.rs","crates/harness-agents/src/runtime_fingerprint/probe.rs","crates/harness-agents/src/runtime_fingerprint/tests.rs","crates/harness-core/src/stack/fingerprint.rs","crates/harness-core/src/stack/fingerprint/model.rs","crates/harness-core/src/stack/fingerprint/schema.rs","crates/harness-core/src/stack/fingerprint/tests.rs","crates/harness-core/src/stack/mod.rs","crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012","B-013","B-014","B-015","B-016"]}
-->

## Current System and Root Cause

ASC-001 is implemented in `harness_core::stack`. It provides the closed
component vocabulary, validated source locators and component IDs, exact
SHA-256 values, observation/trust/freshness fields, and strict JSON parsing.
GH-1733 must consume that contract; it must not redefine or weaken it.

PR #1859 at head `c09002963031abdaf296922c1209df47bf0e97f4`
currently changes four files and provides an unapproved first implementation.
It has seven unresolved, non-outdated review findings:

1. `codex.cloud.setup_secret_env` is treated as runtime environment input, so
   a setup-only secret such as `NPM_ACCESS` can be hashed and passed to the
   version child.
2. every MCP tool is assigned runner ownership even when its persisted source
   is repository, user-global, admin, system, or runtime owned;
3. the default bare commands `codex` and `claude` are deliberately not
   resolved through `PATH`, so ordinary CLI upgrades are not fingerprinted;
4. a timed-out `command.output()` future drops a live child without an
   explicit kill-and-reap lifecycle;
5. schema canonicalization treats a keyword-shaped key inside annotation data
   as a schema keyword and can collapse behavior-distinct defaults;
6. stdout and stderr are fully buffered before the configured output limit is
   applied; and
7. clearing the child environment also removes `PATH`, breaking qualified
   `#!/usr/bin/env` launchers.

The same implementation also forces runtime components to runner ownership,
accepts arbitrary string runtime kinds, and hex-encodes invalid identities into
apparently valid ASC-001 locators. A successful blank or unparsable version can
produce no version and no failure. MCP descriptions are whitespace-normalized
despite being model-visible contract text. Executable metadata, a synchronous
read of up to 64 MiB, and process execution are separate path operations, so a
fingerprint can combine facts from different files and block an async worker.
The outer schema version is exported but absent from the wire contract. Finally,
there is no production consumer, which is correct only when stated as the
producer-only boundary required by B-016.

The root cause is not one isolated bug. The implementation lacks four explicit
boundaries: a strict typed wire model, a source-preserving producer input, a
single supervised executable-observation lifecycle, and a context-aware schema
canonicalizer. This specification replaces those boundaries before #1859 may
be amended. It does not approve that implementation or advance issue readiness.

## Module and Ownership Design

Keep the public modules `harness_core::stack::fingerprint` and
`harness_agents::runtime_fingerprint`, but split their private implementation
before adding the remediation:

- `stack/fingerprint.rs` is the public facade for envelope construction,
  parsing, validation, and canonical payload digesting.
- `stack/fingerprint/model.rs` owns the closed subjects, payloads, runtime
  kinds, environment facts, version facts, failure vocabulary, and typed
  errors.
- `stack/fingerprint/schema.rs` owns duplicate-aware JSON decoding and the
  schema-context canonicalization state machine.
- `stack/fingerprint/tests.rs` owns core wire, MCP, schema, and digest tests.
- `runtime_fingerprint.rs` owns configured-runtime inputs and the async
  orchestration that assembles one runtime payload.
- `runtime_fingerprint/environment.rs` owns typed declarations, setup-secret
  exclusion, probe environment construction, and environment evidence.
- `runtime_fingerprint/executable.rs` owns native command resolution,
  handle-based inspection, bounded hashing, and path-identity checks.
- `runtime_fingerprint/probe.rs` owns process supervision, combined output
  draining, exit classification, and version parsing.
- `runtime_fingerprint/tests.rs` owns PATH, identity, environment, lifecycle,
  and failure regressions.

Every production file and test file must remain below 800 lines after rustfmt;
the typical target is 200-400 lines. `stack/mod.rs` and `harness-agents/lib.rs`
only expose the two facades. The one authorized server-file change is a
`#[cfg(test)]` contract test described below; it adds no production import,
mapping, call site, or consumer. No manifest or dependency change is required.

## Strict Fingerprint Envelope

`harness-core` owns the complete public wire model so both subjects use the
same parser and cannot disagree about the outer schema. The conceptual shape
is:

```text
AgentStackFingerprintEnvelope
  schema_version: "agent-stack-fingerprint/v0.1"
  subject: AgentRuntime | McpTool
  component: AgentStackComponent
  payload: RuntimeExecutableFingerprintPayload | McpToolFingerprintPayload
```

The payload variants have fixed inner schema versions
`runtime-executable-fingerprint/v0.1` and `mcp-tool-fingerprint/v0.1`.
Implement the subject/payload pair as a closed Rust enum with private fields,
validated constructors, read-only accessors, and custom strict wire conversion.
Do not expose `serde_json::Value`, generic `Any`, a public payload trait, or an
open string discriminator. Wire structs use `deny_unknown_fields`.

`from_json_str` first distinguishes JSON syntax/shape failure from typed domain
failure, then validates, in order:

1. exact outer version and closed subject;
2. matching typed payload and exact inner version;
3. the ASC-001 component, subject/kind agreement, and an empty capability list;
4. exact B-003 observation, trust, selection, and freshness values;
5. payload-local ordering and impossible-state invariants; and
6. component integrity equality with the recomputed canonical payload digest.

An `agent_runtime` envelope cannot contain an MCP payload, and vice versa.
Constructors always emit an empty component capability list, and parsing
rejects any nonempty list because this producer records executable/tool
identity rather than declared, granted, or observed capability evidence.
Expected probe failures are valid runtime envelopes only when their failure and
missing-fact matrix is valid. Invalid producer input returns a typed error and
does not construct an envelope. This implements B-001, B-014, and B-015.

Canonical payload hashing uses a domain-separated object containing the exact
inner schema version and every typed payload field. JSON object keys are sorted
lexicographically and typed collections use their specified stable order. The
hash excludes the outer component, observation timestamp, run identity, raw
diagnostics, and secret values. The resulting `Sha256Digest` is installed as
the component integrity only after validation, avoiding self-reference.

## Closed Local Runtime Identity

Add a closed `LocalExecutableRuntimeKind` with exactly:

| Value | Fixed version invocation |
| --- | --- |
| `codex_exec` | `--version` |
| `codex_jsonrpc` | `--version` |
| `claude_code` | `--version` |

The public configured-runtime constructor takes this enum, a validated
`AgentStackSource`, and exactly one `PathBuf`. It has no arbitrary `new(String,
...)`, arbitrary argument vector, shell string, alias parser, or pre-encoding
hook. `anthropic_api` and `remote_host` have no conversion. Fixed version
arguments are private data derived exhaustively from the enum.

`configured_runtime_executables_from_agents_config` produces the two distinct
Codex roles and the Claude role with explicit persisted source bindings. It
does not invent a runner source when ownership is absent, and it rejects
duplicate component IDs across those bindings. Callers that cannot provide a
validated ownership source get a typed error rather than a generated UUID,
display label, or hex-encoded locator.

The `#[cfg(test)]` addition in
`workflow_runtime_worker/runtime_profile.rs` imports the producer enum only in
the test module and exhaustively matches all five workflow `RuntimeKind`
variants: the three local variants map one-to-one; `AnthropicApi` and
`RemoteHost` map to `None`. Adding a workflow variant therefore breaks this
test until its local-executable status is decided. This preserves dependency
direction, adds no runtime consumer, and covers B-002 and B-016.

## Source-Preserving Producers

Observation and ownership remain separate at both producer boundaries.

For runtimes, `ConfiguredRuntimeExecutable` retains its caller-supplied
validated source verbatim. The emitted component is `agent_runtime` with that
source. For MCP tools, the constructor accepts an existing validated
`mcp_server` component, a separately validated stable tool source, the exact
advertised tool name, optional exact description, and input schema. It requires
the server and tool ownership scopes to agree and derives the tool component ID
only through `AgentStackComponentId::from_source(McpTool, tool_source)`.
Server identity in the payload is the exact validated server component ID, not
an arbitrary display string.

Both components use `runner_observed`, `runner_observed`, `observed`, and
`fresh`, while retaining the supplied scope and locator. Blank tool names,
wrong server kind, mismatched or untyped ownership, UUID/display/per-observation
source locators, and duplicate component identity fail before hashing. No
`stable_logical_segment` or equivalent encoder may turn invalid input into a
valid source. Fixtures cover every ASC-001 source scope and freeze the component
ID before and after runner observation. This implements B-003 and B-011.

Tool name and description are copied as exact UTF-8 strings. The producer does
not call `trim`, `split_whitespace`, Unicode normalization, case conversion, or
punctuation rewriting. `None`, `Some("")`, spaces, tabs, and newlines remain
distinct serialized facts and digest inputs, as required by B-012.

## Single-Command PATH Resolution

`executable.rs` resolves one configured command without a shell:

- absolute paths are used as the sole candidate;
- relative paths containing a directory component are joined only to the
  declared working directory;
- bare names traverse the supplied sanitized native `PATH` in native order and
  inspect only that exact basename (and native `PATHEXT` forms on Windows);
- missing, non-file, and non-executable candidates are skipped only where the
  platform launch contract would skip them; once the launch-selected candidate
  is chosen, later entries are never fallback probe targets; and
- quotes, spaces, pipes, substitutions, and redirections are literal path
  characters. No `sh`, `which`, package manager, or candidate execution is
  permitted during resolution.

Empty/native-relative PATH entries retain platform launch meaning relative to
the declared working directory. The exact native PATH bytes are
domain-separated by platform and represented only by SHA-256 plus the
resolution outcome; directory contents and raw PATH text are never serialized.
A selected path that cannot be represented in the strict fingerprint identity
records `path_unusable` without a lossy placeholder. This implements B-004 and
the PATH portion of B-010.

The probe command receives exactly the same sanitized PATH value used by the
resolver, so a qualified `#!/usr/bin/env node` launcher observes the same
interpreter search. Other environment keys are supplied only by the typed
policy below. Resolution never claims which executable a later adapter run
will select.

## Typed Environment Policy

Replace string-key heuristics with validated declarations:

```text
RuntimeEnvironmentDeclaration
  key: validated nonblank, NUL-free key
  sensitivity: Public | Sensitive
  probe_exposure: Excluded | Exposed
```

Declarations describe behavior-affecting runtime keys only, are unique after
exact key comparison, and serialize in key order. `Public` set values become a
SHA-256 digest; `Sensitive` set values become `redacted` with no value digest;
missing values become `unset`. Probe exposure is independent: only an explicit
`Exposed` declaration adds a value to the minimal version environment. Raw
values and undeclared keys never enter the envelope.

`codex.cloud.setup_secret_env` is a separate typed exclusion set, not a source
of runtime declarations. Every listed key is removed from evidence and the
child environment before any name or value classification. A declaration that
conflicts with that set returns a typed producer-input error; it is never
downgraded to public based on spelling. The injected observation environment
may contain setup values, but neither the payload nor probe can observe them.
PATH uses the special digest/resolution treatment above and cannot be added a
second time as a general declaration. These rules implement B-005 and B-010.

## Handle-Based Executable Observation and TOCTOU Policy

After resolution, one blocking inspection closure opens the selected target
once and operates on that file handle. It obtains handle metadata, proves the
target is a regular executable file, and incrementally hashes fixed-size chunks.
It checks the configured byte limit against initial metadata and again while
reading, stops at `limit + 1`, does not preallocate the maximum, and returns
only bounded typed facts. The closure runs through `spawn_blocking`; no
multi-megabyte file read occurs on a Tokio worker and no `std::fs::read` whole-
file allocation is allowed.

The observation retains the strongest stable handle identity exposed by the
platform (for example device/inode on Unix and the corresponding native file
identity on Windows). Immediately before spawn and again after the child is
reaped, it opens the resolved path and compares that identity with the retained
handle. A symlink is identified by the opened target, not by mixing link
metadata with target bytes.

If either comparison fails or the identity changes, the envelope records
`identity/identity_changed`, emits no version fact, and does not associate the
version output with the inspected executable digest. A pre-spawn mismatch
prevents the probe; a post-spawn mismatch discards the candidate version. The
record may retain only facts explicitly identified as coming from the opened
handle. It cannot claim that pathname execution is race-free: the checks are a
TOCTOU detector and provenance boundary, not cryptographic attestation. This
implements B-006 and its non-goal.

## Supervised Version Probe

`probe.rs` reuses the crate's existing process-group and `ManagedChild`
supervision instead of introducing another unmanaged child type. The command
has null stdin, piped stdout/stderr, `kill_on_drop(true)`, and a dedicated Unix
process group where supported. `ManagedChild` already kills and schedules
reaping on cancellation; the probe adds its own explicit deadline and bounded
dual-stream collector.

The collector reads both pipes concurrently in fixed-size chunks while one
counter enforces `max_output_bytes` across both buffers. It never calls
`Command::output` or `wait_with_output`. If the next bytes would cross the
combined limit, a bounded prefix only is retained for diagnostics outside the
canonical payload, the process group is terminated, both pipes are drained to
EOF after termination, and the root child is awaited. Timeout and read-error
paths use the same terminate/drain/wait sequence. Root exit is not completion
until both pipes close and remaining descendants covered by the process group
are cleaned up. No lifecycle failure can produce a version fact.

The canonical failure record contains only closed enums and compatible bounded
details: an exit code, byte limit, or timeout milliseconds where applicable.
It never contains `io::Error` text, localized diagnostics, raw output, raw
paths, or environment values. Define closed `RuntimeProbePhase` and
`RuntimeProbeFailureKind` enums for every row in the B-008 table. Constructors
validate legal phase/kind/detail combinations. Producers sort by phase rank and
kind rank and reject duplicates; parsers reject unknown or noncanonical input.

The result-state matrix is fail closed:

| Earliest outcome | Allowed later facts |
| --- | --- |
| path resolution failure | no resolved identity, executable digest, or version |
| identity failure | bounded opened-handle facts only; no stable executable digest/version pair |
| spawn/lifecycle/exit/output failure | stable identity may remain; version is absent |
| `identity_changed` after exit | candidate output/version is discarded |
| success | stable identity, zero exit, two exact output digests, selected stream, one normalized version, and no failures |

This implements B-007, B-008, and B-015.

## Version Output Contract

Within the combined bound, stdout and stderr remain separate exact byte
sequences and each receives a SHA-256. Stdout is selected when it contains a
non-ASCII-whitespace byte; otherwise stderr is selected. Successful version
evidence requires both complete streams to be valid UTF-8 and the child to exit
zero.

The parser scans token boundaries without regex or a new dependency. A token
has one optional leading `v` or `V`, at least two dot-separated numeric
components, and optional ASCII SemVer-style prerelease/build suffixes. It
removes only the single leading `v`/`V`; digits, suffix spelling, and suffix
case are retained exactly. Repeated occurrences of the same normalized token
are one candidate; two distinct candidates are `ambiguous_version`.

Zero exit with two blank streams yields `empty_output`; nonblank output with no
candidate yields `unparseable_version`; invalid UTF-8 yields `invalid_utf8`.
Nonzero and signal exits yield their exact closed failure kinds and are not
parsed into success. The payload records the selected stream plus both exact
digests only on success. This implements B-009.

## Context-Aware MCP Schema Canonicalization

`McpInputSchema::from_json_str` uses a duplicate-detecting serde visitor rather
than first decoding to `serde_json::Value`, because the latter can overwrite an
earlier duplicate key. Malformed JSON and duplicate-object-key errors remain
typed and occur before canonicalization or digesting. `from_serializable`
starts from an already typed Rust value and shares the same canonical state
machine after serialization.

The private state machine has these contexts:

- `Schema`: an object whose keys may be JSON Schema keywords;
- `SchemaMap`: values under `$defs`, `definitions`, `properties`,
  `patternProperties`, and `dependentSchemas` are schemas;
- `SchemaArrayOrdered`: `prefixItems` and legacy tuple-form `items` traverse
  elements as schemas but preserve array order;
- `SchemaArraySet`: `allOf`, `anyOf`, and `oneOf` traverse elements as schemas
  and sort their canonical bytes; and
- `InstanceData`: annotation and unknown-extension objects sort object members
  but preserve arrays recursively.

At `Schema`, `required`, `type`, and `enum` arrays are canonical sets. `enum`
elements themselves enter `InstanceData`, so an object inside an enum value
cannot activate schema keywords. `default`, `const`, `examples`, and `example`
always enter `InstanceData`. Known single-schema locations such as `not`,
`if`, `then`, `else`, `contains`, `propertyNames`, `additionalProperties`,
`unevaluatedItems`, and `unevaluatedProperties` enter `Schema`. Unknown and
vendor-extension values enter `InstanceData`. Object members are sorted in all
contexts, but an array is sorted only at the six closed B-013 locations.

Set sorting uses canonical JSON bytes after child traversal and does not silently
remove duplicates. Ordered arrays retain their exact order. Tests specifically
place `enum`, `required`, and `oneOf` keys inside nested annotation objects to
prove the context cannot leak. This implements B-013 and the MCP portion of
B-014.

## Producer-Only Boundary

This issue stops after deterministic constructors, parsers, and contract tests.
There is no call from `CodeAgent`, `AgentAdapter`, `RuntimeKind` dispatch,
workflow runtime, task runner, server startup, snapshot assembly, CLI, or HTTP.
The server contract test is compiled only under `cfg(test)` and constructs no
fingerprint. No persistence or migration is added. Product and code comments
must say the API “can produce” fingerprints, never that Harness collected,
persisted, or used them. ASC-005 owns the first snapshot consumer and ASC-026
owns user-facing collection commands. This is B-016.

## Product-to-Test Mapping

| Product behavior | Required verification |
| --- | --- |
| B-001, B-014, B-015 | `envelope_round_trips_both_closed_subjects`; `envelope_rejects_version_subject_payload_capability_and_integrity_mismatch`; `payload_digest_is_canonical_and_component_free` |
| B-002 | `local_executable_runtime_kind_is_closed_and_uses_fixed_args`; server `runtime_fingerprint_runtime_kind_contract_is_exhaustive` |
| B-003, B-011 | `runner_observation_preserves_every_runtime_and_mcp_source_identity`; `mcp_tool_requires_typed_matching_server_and_tool_ownership` |
| B-004 | `bare_path_resolution_matches_first_native_launch_candidate`; `qualified_relative_and_metacharacter_paths_are_literal`; `resolver_never_inspects_or_executes_unrelated_commands` |
| B-005, B-010 | `setup_secret_env_is_absent_from_probe_and_facts`; `typed_runtime_environment_records_set_unset_digest_and_redacted`; `environment_rejects_duplicates_invalid_keys_and_setup_conflicts` |
| B-006 | `opened_handle_drives_metadata_and_incremental_hash`; `executable_growth_crossing_limit_is_explicit`; `hashing_runs_off_the_async_worker`; `path_replacement_discards_version_with_identity_changed` |
| B-007 | `timeout_kills_and_reaps_probe_group`; `cancellation_kills_and_reaps_probe_group`; `dual_stream_limit_is_combined_and_bounded`; `pipe_read_failure_terminates_and_reaps` |
| B-008 | `failure_vocabulary_round_trips_every_legal_pair`; `failure_order_and_details_are_canonical_and_redacted`; `unknown_or_incompatible_failure_values_are_rejected` |
| B-009 | `version_parser_accepts_exact_v01_grammar_and_preserves_suffix_case`; `stdout_stderr_and_output_digests_are_exact`; `blank_unparseable_ambiguous_invalid_utf8_nonzero_and_signal_are_failures` |
| B-012 | `mcp_description_preserves_absent_empty_space_tab_and_newline_distinctions` |
| B-013 | `schema_set_locations_reorder_canonically`; `ordered_schema_annotation_and_extension_arrays_remain_sensitive`; `schema_keyword_shaped_annotation_keys_remain_instance_data`; `duplicate_json_keys_fail_before_digest` |
| B-016 | `git diff` manifest check plus `rg` call-site audit proving no production consumer |

All failure tests assert the absence of a version fact and the absence of raw
path, PATH, output, environment, and OS-diagnostic text from serialized
evidence. Lifecycle tests retain child PIDs/process-group IDs and verify that
they are gone after the API returns. PATH tests create multiple same-basename
candidates, a directory containing spaces, literal shell metacharacters, and a
qualified `/usr/bin/env`-style test launcher. Schema expected digests are fixed
independent vectors rather than values generated by the production helper
under test.

## Authorized Implementation Surface

Only these paths are authorized:

1. `crates/harness-core/src/stack/mod.rs`
2. `crates/harness-core/src/stack/fingerprint.rs`
3. `crates/harness-core/src/stack/fingerprint/model.rs`
4. `crates/harness-core/src/stack/fingerprint/schema.rs`
5. `crates/harness-core/src/stack/fingerprint/tests.rs`
6. `crates/harness-agents/src/lib.rs`
7. `crates/harness-agents/src/runtime_fingerprint.rs`
8. `crates/harness-agents/src/runtime_fingerprint/environment.rs`
9. `crates/harness-agents/src/runtime_fingerprint/executable.rs`
10. `crates/harness-agents/src/runtime_fingerprint/probe.rs`
11. `crates/harness-agents/src/runtime_fingerprint/tests.rs`
12. `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs`
    (`#[cfg(test)]` exhaustive mapping contract only)

Moving the two existing inline test modules into their listed test files is
part of this scope. No Cargo manifest, lockfile, database, configuration,
adapter, spawn contract, workflow model, CLI, HTTP, prompt, snapshot, or
high-context file change is authorized. Any production server import or call
site requires an ASC-005 consumer specification rather than an amendment here.

## Verification and Handoff Gates

During implementation, use focused commands for the edited crate. Before
commit and push, the exact implementation head must pass:

```text
cargo fmt --all
cargo fmt --all -- --check
cargo check -p harness-core -p harness-agents --all-targets
cargo test -p harness-core fingerprint
cargo test -p harness-core stack
cargo test -p harness-core
cargo test -p harness-agents runtime_fingerprint
cargo test -p harness-agents
cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

The changed-file audit must equal the twelve-path manifest. A call-site audit
must show that production uses of the new APIs remain confined to their
defining modules; test uses do not count as consumers. File-length checks must
show every Rust file below 800 lines. Because the repository no longer contains
the historical SpecRail checker, verification must not claim to run a removed
script; structural review of the manifest and B-001 through B-016 coverage is
the current spec check.

PR #1859 may be amended on its original branch only after this spec packet is
approved and maintainers record `ready_to_implement`. Then every valid review
finding must be addressed and resolved, the branch must be updated to current
`main`, and fresh current-head CI, independent review, Gemini review, and
repository ruleset approval must pass before merge. This spec does not itself
approve implementation, resolve a thread, close GH-1733, or authorize bypass of
those gates.

## Rollout and Rollback

The first release is additive producer-only library code. It has no feature
flag because it has no production caller, persistence, migration, network
operation, or execution-path effect. Rollback before ASC-005 is a straight
revert of the authorized modules and test-only export/mapping changes. There is
no data to migrate or clean up.

If ASC-005 or ASC-026 later consumes these envelopes, that consumer must be
removed or downgraded before removing v0.1 producers. Persisted future snapshots
must remain readable under their own schema contract; this issue does not grant
permission to delete or reinterpret them. Rollback must never convert a failed
probe into a success placeholder, rewrite component ownership to runner, or
fall back to the unsafe pre-spec PATH/environment behavior.

## Risks and Rejected Alternatives

- Reusing arbitrary strings for runtime kinds or hex-encoding them is rejected:
  it launders unstable identities and defeats ASC-001's closed vocabulary.
- Forcing runner source is rejected: observer trust is not component ownership.
- Calling `which`, a shell, or every PATH candidate is rejected: it expands
  authority and differs from one-command adapter launch semantics.
- Name-based secret detection is rejected: setup-secret provenance and typed
  sensitivity are the only accepted classifications.
- `command.output()` followed by truncation is rejected: it does not bound
  memory and cannot supervise both pipes and descendants safely.
- Hashing by path before probing by path without handle identity checks is
  rejected: it can attribute a version to different bytes.
- Globally sorting arrays by keyword-shaped parent names is rejected: it erases
  ordered annotation and extension data.
- Normalizing MCP description whitespace is rejected: model-visible exact text
  is behavior-affecting evidence.
- Adding a snapshot or server collector to make the API appear used is rejected:
  ASC-005 owns that consumer and its aggregation semantics.

The residual TOCTOU gap between the final pre-spawn check and OS execution is
explicit and cannot be promoted to attestation. Platform process-group and
file-identity capabilities differ; unsupported guarantees must yield the
applicable typed failure rather than a warning-only fallback.
