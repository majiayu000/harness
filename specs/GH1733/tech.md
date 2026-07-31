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
  parsing, validation, and canonical fingerprint hashing.
- `stack/fingerprint/model.rs` owns the closed subjects, payloads, runtime
  kinds, environment facts, version facts, failure vocabulary, and typed
  errors.
- `stack/fingerprint/schema.rs` owns duplicate-aware JSON decoding and the
  schema-context canonicalization state machine.
- `stack/fingerprint/tests.rs` owns core wire, MCP, schema, and digest tests.
- `runtime_fingerprint.rs` owns configured-runtime inputs and the async
  orchestration that assembles one runtime payload.
- `runtime_fingerprint/environment.rs` owns the closed runtime-kind policy,
  platform key normalization, setup-secret exclusion, probe environment
  construction, and environment evidence.
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
  fingerprint_digest: Sha256Digest
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
6. ASC-001 integrity wire shape, plus the subject-specific invariant that a
   derived MCP tool component has no integrity; and
7. equality between `fingerprint_digest` and the recomputed canonical
   fingerprint digest.

An `agent_runtime` envelope cannot contain an MCP payload, and vice versa.
Constructors always emit an empty component capability list, and parsing
rejects any nonempty list because this producer records executable/tool
identity rather than declared, granted, or observed capability evidence.
Expected probe failures are valid runtime envelopes only when their failure and
missing-fact matrix is valid. Invalid producer input returns a typed error and
does not construct an envelope. This implements B-001, B-014, and B-015.

Canonical fingerprint hashing uses a domain-separated object containing the
exact subject, inner schema version, and every typed payload field. JSON object
keys are sorted lexicographically and typed collections use their specified
stable order. The hash excludes the outer component, observation timestamp, run
identity, raw diagnostics, and secret values. The resulting `Sha256Digest` is
stored only in `fingerprint_digest`. It never replaces ASC-001 component
integrity, which remains evidence about exact component source bytes or is
absent. The envelope parser cannot attest which bytes a producer hashed because
wire input carries no source bytes; exact-byte correspondence is established
only by typed producer construction and its tests. This avoids self-reference
while preserving the two distinct claims.

## Closed Local Runtime Identity

Add a closed `LocalExecutableRuntimeKind` with exactly:

| Value | Fixed version invocation | Whole-output grammar |
| --- | --- | --- |
| `codex_exec` | `--version` | `codex-cli <VERSION>` |
| `codex_jsonrpc` | `--version` | `codex-cli <VERSION>` |
| `claude_code` | `--version` | `<VERSION> (Claude Code)` |

The public configured-runtime constructor takes this enum, a validated
`ConfiguredRuntimeSource`, the existing
`harness_core::config::isolation::IsolationTier`, and exactly one `PathBuf`.
An exhaustive match accepts only `Host`; `Container` and `Microvm` return a
typed producer-input error before PATH resolution, file access, or process
creation.
Their configured CLI path names a command inside another execution boundary,
not the host executable that would actually be launched, so host
fingerprinting would be false evidence. The constructor has no arbitrary
`new(String, ...)`, arbitrary argument vector, shell string, alias parser, or
pre-encoding hook. `anthropic_api` and `remote_host` have no conversion. Fixed
version arguments and output grammars are private data derived exhaustively
from the enum, and successful v0.1 payloads record `execution_isolation: host`.

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

For runtimes, a closed `ConfiguredRuntimeSource` retains the caller's validated
`AgentStackSource` plus optional exact source bytes. Its constructors are
`without_canonical_bytes(source)` and `from_exact_source_bytes(source, bytes)`;
the latter computes ASC-001 integrity internally, and there is no bare-digest
setter. `ConfiguredRuntimeExecutable` emits an `agent_runtime` component with
that source and either the exact-byte integrity or absence. Executable and
payload digests never overwrite it. For MCP tools, the constructor accepts an
existing validated `mcp_server` component, the exact advertised tool name,
optional exact description, and raw input-schema JSON. It accepts no separate
tool source.

A private typed `McpToolSource::derive` mapping preserves the server source
scope and appends
`harness_mcp_tool_v0_1/u<byte_length>_<lowercase_utf8_hex>` to the server
locator. The exact UTF-8 byte length and lowercase hexadecimal bytes make the
mapping reversible, injective, case-sensitive, and distinct for every tool on
one server. Callers cannot provide or pre-encode this locator. Server identity
in the payload is the exact validated server component ID, not an arbitrary
display string. Because this binding is derived structured identity rather
than canonical raw source bytes, the tool component integrity is absent.

Both components use `runner_observed`, `runner_observed`, `observed`, and
`fresh`, while retaining the supplied or derived scope and locator. Blank tool
names, wrong server kind, generated per-observation server identity,
UUID/display ownership, malformed derived locators, caller-supplied encodings,
and duplicate component identity fail before hashing. No
`stable_logical_segment` or caller-facing encoder may turn invalid input into a
valid source. Fixtures cover every ASC-001 source scope, multiple exact tool
names on one server, and component IDs before and after runner observation.
This implements B-003 and B-011.

Runtime version-probe authorization is a separate private closed mapping from
`AgentStackSourceScope`. `Repository` maps to `IdentityOnly`: resolution,
handle inspection, and hashing may run, but the payload records
`version_probe/probe_not_authorized` and no process is created. `UserGlobal`,
`Admin`, `System`, `Runtime`, and genuine `Runner` map to
`VersionProbeEligible`. There is no caller boolean, string trust level, or
builder that can promote a repository source. This execution policy does not
reinterpret ASC-001 observation or trust metadata.

Tool name and description are copied as exact UTF-8 strings. The producer does
not call `trim`, `split_whitespace`, Unicode normalization, case conversion, or
punctuation rewriting. `None`, `Some("")`, spaces, tabs, and newlines remain
distinct serialized facts and digest inputs, as required by B-012.

## Single-Command PATH Resolution

`executable.rs` resolves one configured command without a shell and pins the
Rust toolchain behavior used by the adapter. The input carries a typed
`RuntimeLaunchContext`: platform, configured child working directory,
sanitized child `PATH`, and every platform search base that the resolver needs
but must not infer.

The first operation exhaustively matches the existing `IsolationTier`. Any
non-host tier returns its typed producer-input error before the resolver
observes PATH or other launch context, opens a file, or attempts a process.
This ordering is a tested privacy and evidence boundary, not merely an
unsupported branch later in the probe.

On Unix:

- an absolute path is the sole candidate;
- a qualified relative path is joined to the declared child working directory;
- a bare name traverses sanitized `PATH` in order, with empty and relative
  entries based on the declared child working directory, matching `chdir` then
  `execvp`; and
- execute permission is checked from opened-handle mode bits.

On Windows, the pinned Rust `Command` bare-name resolver is mirrored: explicit
child `PATH`, current executable directory, system directory, Windows
directory, then parent `PATH`, with only Rust's `.exe` completion. `PATHEXT` is
not consulted. Explicit `.bat` and `.cmd` programs are `path_unusable` because
Rust's batch handling invokes a command interpreter and violates the no-shell
boundary. Every explicit non-`.exe` extension is likewise `path_unusable` in
v0.1; supporting another extension requires a later closed grammar revision.
The child `current_dir` is not treated as the parent search base. A qualified
relative program, relative/empty search entry, or unavailable special directory
is accepted only when its actual base is explicit and stable in the launch
context; otherwise resolution records `path_unusable`. Tests freeze this order
against the pinned toolchain so a Rust upgrade that changes resolution cannot
silently change fingerprints.

On every platform, quotes, spaces, pipes, substitutions, and redirections are
literal path characters. Resolution inspects only the configured basename,
never runs `sh`, `which`, a package manager, or a candidate. Once selected, the
candidate is converted to an absolute path and that path alone is opened and
spawned; no second OS search or later-candidate fallback is possible.

The effective search inputs are domain-separated by platform and represented
only by SHA-256 plus the resolution outcome; directory contents and raw search
text are never serialized. The probe child receives the exact sanitized child
`PATH` from the launch context so a qualified `#!/usr/bin/env node` launcher
uses the declared interpreter search. Other environment keys are supplied only
by the typed policy below. Resolution never claims which executable a later
adapter run will select. This implements B-004 and the PATH portion of B-010.

## Closed Environment Policy

The public runtime input can supply an observation environment, but it cannot
declare keys, sensitivity, evidence inclusion, or probe exposure. A private
exhaustive `LocalExecutableRuntimeKind::environment_policy()` returns exactly:

| Runtime kind | Key | Evidence rule | Probe rule |
| --- | --- | --- | --- |
| `codex_exec`, `codex_jsonrpc` | `OPENAI_API_KEY` | `unset` or `redacted` | excluded |
| `claude_code` | `ANTHROPIC_API_KEY` | `unset` or `redacted` | excluded |
| `claude_code` | `CLAUDE_CONFIG_DIR` | `unset` or exact-value SHA-256 | excluded |
| all three | `PATH` | domain-separated digest plus resolution outcome | exposed only as sanitized launch `PATH` |

No other ordinary key enters evidence or the version child. There is no public
`RuntimeEnvironmentDeclaration`, classification builder, automatic public
fallback, or caller-set exposure flag. The closed table is the only source of
`unset`, `redacted`, and digest facts.

Environment-key normalization occurs before duplicate, `PATH`, policy, and
setup-secret checks. Unix retains exact case-sensitive UTF-8 names. Windows
v0.1 accepts only ASCII names, canonicalizes them to uppercase for comparison,
rejects canonical collisions such as `Path` plus `PATH`, and serializes the
policy spelling from the table. A non-ASCII Windows name fails typed because
the producer cannot reproduce the OS's Unicode case-insensitive comparison
without an authorized platform primitive.

`codex.cloud.setup_secret_env` is a separate exclusion set, not a source of
policy entries. Its names pass through the same platform canonicalizer, and
matching keys are removed from evidence and child input before policy lookup.
Thus setup-secret exclusion overrides even a listed policy key. PATH cannot be
represented a second time as an ordinary entry. Raw values and undeclared keys
never enter the envelope. These rules implement B-005 and B-010.

## Handle-Based Executable Observation and TOCTOU Policy

After resolution, one blocking inspection closure opens the selected target
once and operates on that file handle. It obtains handle metadata, proves the
target is a regular file, and incrementally hashes fixed-size chunks. Unix also
derives execute permission from handle mode bits. Windows extension/search
eligibility belongs to resolution and successful OS loading belongs to spawn;
the producer does not claim that an extension or parsed PE header proves
loadability. On a platform with supervised spawn, a bad-image or access failure
is `spawn_failed`; Windows v0.1 instead stops at `containment_unavailable`
before spawn and makes no loadability claim.

The closure checks the configured byte limit against initial metadata and again
while reading, stops at `limit + 1`, does not preallocate the maximum, and
returns only bounded typed facts. It runs through `spawn_blocking`; no
multi-megabyte file read occurs on a Tokio worker and no `std::fs::read` whole-
file allocation is allowed.

The retained strong identity is device/inode from handle metadata on Unix and
volume serial plus 128-bit `FILE_ID_INFO` from the opened handle on Windows.
Path, mtime, extension, or a weaker optional metadata field is not a fallback.
If the opened handle cannot provide the specified strong identity, the producer
records `identity/metadata_unavailable` before version attribution.
`path_unusable` remains exclusive to resolution. Immediately before spawn and
again after the child is reaped, one blocking checkpoint re-reads and re-hashes
the retained handle and opens the resolved path to compare strong identity.
All three retained-handle size and digest observations must match, and both
later path identities must equal the retained handle. A symlink is identified
by the opened target, not by mixing link metadata with target bytes.

If any size, digest, or strong-identity comparison fails, the envelope records
`identity/identity_changed`, emits no version fact, and does not associate the
version output with the inspected executable digest. A pre-spawn mismatch
prevents the probe; a post-reap mismatch discards the candidate version. The
successful correlation is explicitly named `checkpoint_consistent_path`. It
cannot claim pathname execution is race-free or that the executed bytes equal
the digest: mutation and restoration entirely between checkpoints remains a
residual TOCTOU gap. This implements B-006 and its non-goal.

## Supervised Version Probe

`probe.rs` owns a fingerprint-specific `RuntimeFingerprintProbeSupervisor`
instead of treating the existing `ManagedChild` drop path as completion
evidence. On Unix the command has null stdin, piped stdout/stderr, and a
dedicated process group created before exec. The supervisor owns the lifecycle
before spawn. This is deliberately named process-group supervision, not
descendant containment: a Unix child may call `setsid` or change process groups,
so the producer can prove only root status and observations about processes
that remain in the original group. It never emits a whole-descendant-tree-empty
claim. A non-escapable Linux/macOS sandbox is outside this packet, and the
closed source policy prevents repository-owned code from reaching this probe.

Every explicit timeout, overflow, or read-error path attempts, under one
private nonzero five-second monotonic
`RUNTIME_FINGERPRINT_CLEANUP_DEADLINE`, to signal the negative process-group
ID, drain both pipes within the remaining budget, reap the root, and verify the
original group is empty. If all operations complete, the envelope carries only
the triggering `version_probe` failure. If an operation fails or the deadline
expires, the envelope also carries the applicable closed
`lifecycle_cleanup` failure:

| Cleanup kind | Trigger |
| --- | --- |
| `termination_failed` | signalling the root/original group failed |
| `output_drain_failed` | bounded drain did not complete before read handles were closed |
| `reap_failed` | root reap failed or was not verified |
| `group_verification_failed` | original-group emptiness was not verified |

Original trigger failures sort before cleanup failures. Any cleanup failure
closes parent read handles, omits version and any terminated/reaped or
whole-tree claim, and transfers child/group ownership to a
fingerprint-specific runtime-independent owner before returning evidence. The
owner emits an `error`, keeps ownership, and continues signalling, reaping, and
original-group verification; later success does not rewrite an already emitted
fingerprint. This prevents an escaped pipe holder from blocking the API
indefinitely while remaining honest about the limited process-group evidence.

Caller cancellation synchronously signals the original process group and uses
the same ownership transfer, but emits no envelope. The owner survives
immediate shutdown of the Tokio runtime that hosted the cancelled future. If
starting the cleanup thread fails, the drop path logs an error and performs
synchronous blocking termination and reap rather than abandoning ownership.
Root-only `kill_on_drop` and the existing detached Tokio `ManagedChild` reaper
are not completion evidence.

The current non-Unix `ManagedChild` has no equivalent pre-spawn process-group
or Job Object supervision. Windows v0.1 therefore records
`version_probe/containment_unavailable` before spawning and emits no version
fact. The frozen failure name means the required supervision primitive is
unavailable; it does not imply that Unix process groups are non-escapable.
Assigning a Job Object after `Command::spawn` is insufficient. Atomic Windows
supervision would require suspended low-level launch, no-breakaway
kill-on-close Job Object assignment, then resume, which is outside this packet.

The collector reads both pipes concurrently in fixed-size chunks while one
counter enforces an inclusive `max_output_bytes` across both buffers. It reads
at most the remaining capacity plus one sentinel byte and never calls
`Command::output` or `wait_with_output`. Exactly the configured maximum is
legal; observing byte `max_output_bytes + 1` records
`output_limit_exceeded`. A bounded prefix only is retained for diagnostics
outside the canonical payload. Cleanup uses the remaining shared deadline and
may close read handles rather than waiting forever for EOF. No lifecycle or
cleanup failure can produce a version fact.

The canonical failure record contains only closed enums and compatible bounded
details: an exit code, byte limit, timeout milliseconds, or closed cleanup
operation where applicable.
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
| repository probe not authorized | stable identity/hash may remain; no child was spawned and version is absent |
| supervision unavailable | stable identity may remain; no child was spawned and version is absent |
| spawn/lifecycle/exit/output failure | stable identity may remain; version is absent |
| cleanup failure | stable identity may remain; version and completed-cleanup claims are absent; independent owner continues |
| `identity_changed` after exit | candidate output/version is discarded |
| caller cancellation | no envelope; cleanup ownership survives Tokio shutdown and is never abandoned |
| success | stable identity, zero exit, two exact output digests, selected stream, one normalized version, and no failures |

This implements B-007, B-008, and B-015.

## Version Output Contract

Within the combined bound, stdout and stderr remain separate exact byte
sequences and each receives a SHA-256 before parsing. Successful version
evidence requires both complete streams to be valid UTF-8 and the child to exit
zero.

The closed runtime kind selects a whole-stream grammar. Codex Exec and Codex
JSON-RPC accept exactly `codex-cli <VERSION>`; Claude Code accepts exactly
`<VERSION> (Claude Code)`. Each permits only one optional final LF or CRLF.
`VERSION` is ASCII SemVer with exactly three numeric core components, no
invalid leading zero, and optional prerelease/build suffix. Its exact spelling
and suffix case are retained. A leading `v`/`V`, surrounding whitespace, extra
line, dependency/runtime suffix, or partial token match is rejected rather than
guessed.

Both complete streams are parsed independently before selection:

| Stdout | Stderr | Result |
| --- | --- | --- |
| matching product line | ASCII blank | select stdout |
| ASCII blank | matching product line | select stderr |
| matching product line | matching product line | `ambiguous_version`, even when the version text is equal |
| matching product line | nonblank invalid | `unparseable_version` |
| nonblank invalid | matching product line | `unparseable_version` |
| ASCII blank | ASCII blank | `empty_output` |
| any other nonblank combination | any | `unparseable_version` |

Invalid UTF-8 yields `invalid_utf8`. Nonzero and signal exits are not parsed
into success. The payload records the selected stream plus both exact digests
only on success. Changing a product output grammar requires a new schema
grammar revision, not a heuristic first-token fallback. This implements B-009.

## Context-Aware MCP Schema Canonicalization

`McpInputSchema` exposes only `from_json_str` and `from_json_slice`. Both start
from raw JSON and use a duplicate-detecting serde visitor rather than first
decoding to `serde_json::Value`, because the latter can overwrite an earlier
duplicate key. Malformed JSON and duplicate-object-key errors remain typed and
occur before canonicalization or digesting. There is no public
`from_serializable`, `serde_json::Value`, or typed-map evidence constructor:
after ordinary decoding, original duplicate-key absence cannot be attested.

The parser and canonicalizer enforce fixed v0.1 resource limits:

| Resource | Inclusive maximum |
| --- | ---: |
| tool-name UTF-8 bytes | 1,024 |
| description UTF-8 bytes | 65,536 |
| raw schema bytes | 1,048,576 |
| schema nesting depth | 64 |
| total JSON nodes | 65,536 |
| cumulative decoded string bytes | 1,048,576 |
| entries in one object or array | 4,096 |
| canonical schema bytes | 1,048,576 |

The raw-byte check occurs before parse. The duplicate-aware visitor counts
depth, nodes, decoded string bytes, and per-container entries while building
the private representation. Canonicalization rechecks depth and nodes, charges
every intermediate canonical byte before set sorting, and rejects budget
overflow before allocating an unbounded sort key. Exact limits are legal;
limit-plus-one returns a closed typed `McpContractLimitKind` error and emits no
digest or envelope. Depth 64 keeps recursive traversal below the fixed safe
bound without a new parser or dependency.

The private state machine has these contexts:

- `Schema`: an object whose keys may be JSON Schema keywords;
- `SchemaMap`: values under `$defs`, `definitions`, `properties`,
  `patternProperties`, and `dependentSchemas` are schemas;
- `SchemaArrayOrdered`: `prefixItems` and legacy array-form `items` traverse
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
`unevaluatedItems`, and `unevaluatedProperties` enter `Schema`. At `items`, an
object or boolean enters `Schema`, an array enters `SchemaArrayOrdered`, and
any other value is a typed malformed-schema error. Unknown and vendor-extension
values enter `InstanceData`. Object members are sorted in all contexts, but an
array is sorted only at the six closed B-013 locations.

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
| B-001, B-014, B-015 | `envelope_round_trips_both_closed_subjects`; `envelope_rejects_version_subject_payload_capability_and_fingerprint_digest_mismatch`; `fingerprint_digest_is_separate_from_component_integrity`; `failure_payload_changes_fingerprint_digest_without_fabricating_integrity`; `component_integrity_preserves_exact_source_bytes_or_absence` |
| B-002 | `local_executable_runtime_kind_is_closed_and_uses_fixed_args_and_output_grammars`; `container_isolation_fails_before_host_resolution`; `microvm_isolation_fails_before_host_resolution`; server `runtime_fingerprint_runtime_kind_contract_is_exhaustive` |
| B-003, B-011 | `runner_observation_preserves_every_runtime_and_mcp_source_identity`; `repository_owned_runtime_never_spawns_version_child`; `caller_cannot_promote_repository_source`; `mcp_tool_source_is_injective_for_multiple_tools_on_one_server`; `mcp_tool_source_preserves_scope_and_encodes_exact_utf8_identity`; `caller_cannot_supply_preencoded_mcp_tool_source` |
| B-004 | Unix `bare_path_resolution_uses_child_cwd_and_first_path_candidate`; Windows `bare_path_resolution_matches_pinned_rust_order_without_pathext`; `windows_non_exe_programs_are_path_unusable` with explicit `.bat`/`.cmd` no-shell assertions; `unstable_relative_resolution_is_path_unusable`; `resolver_spawns_only_the_selected_absolute_path` |
| B-005, B-010 | `runtime_kind_selects_closed_environment_policy`; `arbitrary_environment_key_cannot_be_declared_or_exposed`; `aws_secret_access_key_never_reaches_probe_or_evidence`; `setup_secret_exclusion_overrides_closed_policy`; `cross_runtime_environment_key_is_excluded`; `windows_path_case_variants_collide`; `windows_setup_secret_exclusion_is_case_insensitive`; `windows_non_ascii_environment_key_fails_closed`; `unix_environment_keys_remain_case_sensitive` |
| B-006 | `opened_handle_drives_metadata_and_incremental_hash`; `unix_execute_bits_come_from_handle`; Windows `strong_file_id_is_required_without_executable_inference`; `executable_growth_crossing_limit_is_explicit`; `hashing_runs_off_the_async_worker`; `path_replacement_discards_version_with_identity_changed`; `in_place_rewrite_before_spawn_discards_version`; `in_place_rewrite_during_probe_discards_version`; `checkpoint_consistency_does_not_claim_executed_digest` |
| B-007 | Unix `ordinary_timeout_reaps_root_and_verifies_original_group`; `process_group_supervision_does_not_claim_non_escapable_containment`; `setsid_descendant_cannot_produce_descendant_tree_empty_evidence`; `escaped_pipe_holder_hits_cleanup_deadline_without_version`; `exact_combined_output_limit_is_allowed`; `combined_output_limit_plus_one_starts_cleanup`; `cancellation_reaps_after_immediate_tokio_runtime_shutdown`; Windows `containment_unavailable_prevents_spawn` |
| B-008 | `failure_vocabulary_round_trips_every_legal_pair`; `timeout_plus_cleanup_failure_round_trips_in_canonical_order`; `cleanup_failure_never_emits_version_or_reaped_claim`; `deadline_expiry_transfers_ownership_and_returns_incomplete_evidence`; `failure_order_and_details_are_canonical_and_redacted`; `unknown_or_incompatible_failure_values_are_rejected` |
| B-009 | `version_parser_accepts_exact_codex_and_claude_whole_stream_grammars`; `version_parser_rejects_v_prefix_extra_text_and_dependency_versions`; `stdout_stderr_and_output_digests_are_exact`; `both_streams_are_parsed_before_selection`; `same_version_on_both_streams_is_ambiguous`; `valid_version_with_nonblank_other_stream_is_unparseable`; `blank_unparseable_ambiguous_invalid_utf8_nonzero_and_signal_are_failures` |
| B-012 | `mcp_description_preserves_absent_empty_space_tab_and_newline_distinctions`; exact-limit and limit-plus-one tool-name/description fixtures |
| B-013 | `schema_set_locations_reorder_canonically`; `ordered_schema_annotation_and_extension_arrays_remain_sensitive`; `schema_keyword_shaped_annotation_keys_remain_instance_data`; `object_form_items_traverses_nested_schema`; `object_form_items_required_and_one_of_reorder_canonically`; `legacy_array_items_preserves_tuple_order`; `boolean_items_is_canonical_schema`; `raw_schema_rejects_duplicate_keys`; exact-limit and limit-plus-one fixtures for every `McpContractLimitKind`; deep/wide input does not panic; `rg` API audit proving no public `from_serializable`, `serde_json::Value`, or typed-map evidence constructor |
| B-016 | `git diff` manifest check plus `rg` call-site audit proving no production consumer |

All failure tests assert the absence of a version fact and the absence of raw
path, PATH, output, environment, and OS-diagnostic text from serialized
evidence. Ordinary explicit lifecycle tests retain child PIDs/process-group IDs
and verify root reap plus original-group emptiness when no cleanup failure is
recorded. Fault-injection tests cover every cleanup operation, transfer
ownership before returning incomplete evidence, and never claim an escaped
descendant was contained. Cancellation tests drop the hosting Tokio runtime
immediately and verify the independent owner continues cleanup. PATH tests
create multiple same-basename candidates, a directory containing spaces, and
literal shell metacharacters.
The qualified `/usr/bin/env`-style child-execution fixture is Unix-only;
Windows tests stop before spawn with `containment_unavailable`. Schema expected
digests are fixed independent vectors rather than values generated by the
production helper under test.

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
  memory and cannot supervise both pipes safely.
- Treating a Unix process group as non-escapable descendant containment is
  rejected: v0.1 records only root and original-group observations.
- Executing repository-owned code merely to obtain `--version` is rejected:
  repository sources produce identity-only evidence with
  `probe_not_authorized` until a separately specified hardened sandbox exists.
- Caller-declared environment sensitivity or exposure is rejected: the closed
  runtime-kind table is the only policy.
- Unbounded MCP parsing/canonicalization is rejected: every v0.1 contract limit
  is fixed and enforced before or during allocation.
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
