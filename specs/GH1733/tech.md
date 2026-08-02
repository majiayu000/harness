# Tech Spec

## Linked Issue

GH-1733

## Product Spec

See `specs/GH1733/product.md` and the split runtime contracts in
`runtime-product.md`, `runtime-observation.md`, and
`runtime-supervision.md`. The execution plan and product-to-test map are in
`tasks.md`.

<!-- specrail-planned-changes
{"issue":1733,"complete":true,"paths":["crates/harness-agents/Cargo.toml","crates/harness-agents/src/lib.rs","crates/harness-agents/src/runtime_fingerprint.rs","crates/harness-agents/src/runtime_fingerprint/environment.rs","crates/harness-agents/src/runtime_fingerprint/executable.rs","crates/harness-agents/src/runtime_fingerprint/probe.rs","crates/harness-agents/src/runtime_fingerprint/tests.rs","crates/harness-core/Cargo.toml","crates/harness-core/src/stack/fingerprint.rs","crates/harness-core/src/stack/fingerprint/model.rs","crates/harness-core/src/stack/fingerprint/schema.rs","crates/harness-core/src/stack/fingerprint/tests.rs","crates/harness-core/src/stack/mod.rs","crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012","B-013","B-014","B-015","B-016"]}
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
  kinds, canonical runtime-role source binding, environment facts, version
  facts, failure vocabulary, and typed errors.
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
mapping, call site, or consumer. `harness-agents` adds a direct dependency on
the already pinned workspace `libc` solely for Linux no-shell descriptor
isolation, exact pidfds, ptrace exec-stop, and
`execveat(AT_EMPTY_PATH)` primitives. This adds no
package/version and must not change `Cargo.lock`. `harness-core` explicitly
enables the existing workspace `serde_json` dependency's `raw_value` feature
so borrowed `RawValue` slices can preserve validated number lexemes. This also
adds no package/version and must not change `Cargo.lock`; no handwritten JSON
lexer is authorized.

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
3. the ASC-001 component, subject/kind agreement, an empty capability list,
   and the subject-specific source binding: a canonical runtime-role locator
   whose decoded suffix equals the runtime payload kind, or a strict
   `McpToolSource::parse` that peels and re-derives the configured-server and
   tool suffixes, reconstructs the server component ID, and requires it plus
   the exact decoded tool name to equal the MCP payload;
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

For schema v0.1, constructors and strict parsing reject every `WindowsBare`,
`WindowsAbsolute`, and `WindowsQualified` runtime command form, as well as any
present Windows resolution context. Supported Windows resolver/digest functions
remain pure contract helpers for a later schema revision; because the v0.1
producer always returns no-envelope
`ContainmentUnavailable(UnsupportedPlatform)` before observation, those helpers
cannot create an otherwise unreachable envelope state.

Canonical fingerprint hashing uses these exact bytes:

```text
SHA-256(
  "harness_agent_stack_fingerprint_digest_v0_1\0"
  || u64be(subject_utf8.len) || subject_utf8
  || u64be(inner_schema_version_utf8.len) || inner_schema_version_utf8
  || u64be(canonical_payload_utf8.len) || canonical_payload_utf8
)
```

Every count is an unsigned fixed-width `u64` in big-endian order and counts
bytes. Canonical payload JSON contains no insignificant whitespace. Object keys
sort by decoded UTF-8 bytes. Arrays preserve typed order except at the closed
B-013 set locations. `null`, booleans, and typed integers use lowercase/minimal
JSON spelling. Strings escape only quote, backslash, and U+0000 through U+001F:
use `\b`, `\t`, `\n`, `\f`, and `\r` for those five scalars and lowercase
`\u00xx` for the others; emit every other Unicode scalar directly as UTF-8 and
never escape slash. The duplicate-aware schema parser retains every validated
raw JSON number token, and canonicalization emits that token unchanged, so
`1`, `1.0`, and `1e0` remain distinct rather than depending on a floating-point
formatter.

For independent framing conformance, canonical payload bytes
`{"a":1,"z":"\n"}` are hex
`7b2261223a312c227a223a225c6e227d`. With subject/version
`agent_runtime` / `runtime-executable-fingerprint/v0.1`, the digest is
`3f45cc1b14c0099eaf056f9475aa210b4f84d45b2a4940ecff35079b3b1611fe`;
with `mcp_tool` / `mcp-tool-fingerprint/v0.1`, it is
`e00eca6b5f5a3fe3494cf590e68ec59f70e40ee54b7f7f42e48756d296fa85d9`.
Tests also pin an independently calculated complete valid payload for each
subject.

The hash excludes the outer component, observation timestamp, run identity,
raw diagnostics, and secret values. The resulting `Sha256Digest` is stored only
in `fingerprint_digest`. It never replaces ASC-001 component integrity, which
remains evidence about exact component source bytes or is absent. The envelope
parser cannot attest which bytes a producer hashed because wire input carries
no source bytes; exact-byte correspondence is established only by typed
producer construction and its tests. This avoids self-reference while
preserving the two distinct claims.

## Closed Local Runtime Identity

Add a closed `LocalExecutableRuntimeKind` with exactly:

| Value | Fixed version invocation | Whole-output grammar |
| --- | --- | --- |
| `codex_exec` | `--version` | `codex-cli <VERSION>` |
| `codex_jsonrpc` | `--version` | `codex-cli <VERSION>` |
| `claude_code` | `--version` | `<VERSION> (Claude Code)` |

The public configured-runtime constructor takes this enum, a validated
`ConfiguredRuntimeSource`, the existing
`harness_core::config::isolation::IsolationTier`, the adapter's effective
`harness_sandbox::SandboxSpec`, and exactly one `PathBuf`. Validation first
exhaustively accepts only `Host`; `Container` and `Microvm` return a typed
producer-input error. It then accepts only `SandboxMode::DangerFullAccess`
with `allowed_write_paths = None`, the exact state for which `wrap_command`
returns `SandboxEngine::None`. Every other mode or narrowed path set returns
typed `SandboxParityUnavailable`. Both gates run before PATH resolution,
working-directory/executable access, or process creation.
Their configured CLI path names a command inside another execution boundary,
not the host executable that would actually be launched, so host
fingerprinting would be false evidence. The constructor has no arbitrary
`new(String, ...)`, arbitrary argument vector, shell string, alias parser, or
pre-encoding hook. `anthropic_api` and `remote_host` have no conversion. Fixed
version arguments and output grammars are private data derived exhaustively
from the enum, and successful v0.1 payloads record `execution_isolation: host`
plus `sandbox_policy: danger_full_access_unrestricted`. No raw allowed path or
project root enters evidence.

`configured_runtime_executables_from_agents_config` produces the two distinct
Codex roles and the Claude role with explicit persisted source bindings. It
does not invent a runner source when ownership is absent. A private
producer calls the core typed
`RuntimeRoleSourceBinding::derive(base_source, runtime_kind)`, which preserves
scope and appends
`harness_agent_runtime_role_v0_1/u<byte_length>_<lowercase_utf8_hex>` using the
exact closed runtime-kind wire bytes. The three derived component IDs are
pairwise distinct even when both Codex roles share one base binding. Callers
cannot supply, pre-encode, or override the role locator; an apparent suffix in
the base is treated as ordinary base input and receives another derived suffix.
Every fingerprint binding first checks the validated ASC-001 base locator
against the fingerprint-local inclusive
`RUNTIME_FINGERPRINT_MAX_BASE_SOURCE_LOCATOR_BYTES = 4_096` UTF-8-byte limit.
This does not redefine global ASC-001 validity; a longer otherwise-valid source
is unsupported by this bounded producer. Every suffix calculation uses
`checked_add`, and every complete runtime/server/tool locator must fit the
inclusive
`RUNTIME_FINGERPRINT_MAX_DERIVED_SOURCE_LOCATOR_BYTES = 8_259` limit before
allocation or copying. The maximum tool locator is reachable exactly:
4,096 base bytes + 38 server-suffix prefix/count bytes + 2,048 stable-key hex
bytes + 29 tool-suffix prefix/count bytes + 2,048 tool-name hex bytes. The
natural runtime and server maxima are 4,159 and 6,182 bytes. Byte 4,097 at the
base or byte 8,260 in a complete parsed locator fails with the matching closed
limit kind.
The typed contract error carries one closed `RuntimeFingerprintLimitKind`:
`ExactSourceBytes`, `BaseSourceLocatorBytes`, `DerivedSourceLocatorBytes`, or
`EnvelopeBytes`.
For raw input, `EnvelopeBytes` is checked first before JSON allocation. After a
bounded decode, strict parsing checks `DerivedSourceLocatorBytes` before suffix
grammar/decoding and recovered `BaseSourceLocatorBytes`. Typed construction
through `from_exact_source_bytes` first checks
`RUNTIME_FINGERPRINT_MAX_EXACT_SOURCE_BYTES = 2_097_152` before copying or
hashing, then `BaseSourceLocatorBytes`, the binding-specific stable-key/tool-
name limit, and checked derived length before suffix allocation or copying.
Construction without exact source bytes begins at `BaseSourceLocatorBytes`.
Callers that cannot provide a validated ownership source get a typed error
rather than a generated UUID, display label, or free-form locator. Core parsing
uses `RuntimeRoleSourceBinding::parse` to strip the final two segments, validate
the base source under the same scope, decode exactly one closed kind, re-derive
the complete locator, and require equality with the payload kind. Missing,
malformed, noncanonical, or wrong-role suffixes fail typed.

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
the latter rejects byte 2,097,153 before copy/hash and computes ASC-001
integrity internally for accepted input. There is no bare-digest setter.
`ConfiguredRuntimeExecutable` asks the core typed binding to derive the
role source above and emits
an `agent_runtime` component with the same exact-byte integrity or absence.
Multiple roles may truthfully share the base exact-source digest; integrity is
not identity and never includes role text, locator, executable bytes, or
payload.

For MCP tools, the constructor accepts a `ConfiguredMcpServerBinding`, exact
advertised tool name, optional exact description, optional raw annotations
JSON, raw input-schema JSON, and optional raw output-schema JSON. Absence of
annotations and absence of `outputSchema` are separate typed payload states,
each distinct from every present object.
It accepts no arbitrary `mcp_server` component or separate server/tool source.
The typed binding contains a validated base ownership source and the exact
nonblank UTF-8 stable key of one persisted MCP configuration entry. Its only
constructor derives
`harness_mcp_server_config_v0_1/u<byte_length>_<lowercase_utf8_hex>` beneath
the base locator. The key has an inclusive 1,024-byte UTF-8 maximum checked
before hex expansion or locator allocation; byte 1,025 fails typed. After
UTF-8 validation, blank means exactly empty or all bytes in HT/LF/CR/SP
(`0x09`, `0x0a`, `0x0d`, `0x20`). No trim or Unicode whitespace predicate is
used; VT, FF, NBSP, and every other valid UTF-8 scalar remain nonblank exact
identity bytes. The exact advertised tool name uses the same predicate. There is no
constructor from a component, display label,
UUID, session ID, or already encoded locator. This binds observations to the
configured key but deliberately does not claim historical proof that an
external caller persisted the same key across earlier observations.

A private typed `McpToolSource::derive` mapping preserves the configured
server source scope and appends
`harness_mcp_tool_v0_1/u<byte_length>_<lowercase_utf8_hex>` to the server
locator. The exact UTF-8 byte length and lowercase hexadecimal bytes make the
mapping reversible, injective, case-sensitive, and distinct for every tool on
one server. Callers cannot provide or pre-encode this locator. Server identity
in the payload is the exact validated server component ID, not an arbitrary
display string. Because this binding is derived structured identity rather
than canonical raw source bytes, the tool component integrity is absent.

Both components use `runner_observed`, `runner_observed`, `observed`, and
`fresh`, while retaining the supplied or derived scope and locator. Blank tool
names or stable keys, wrong server kind, malformed or noncanonical derived
locators, payload/server/tool mismatches, caller-supplied encodings, and
duplicate component identity fail before hashing. Strict envelope parsing
peels and re-derives both suffixes and requires the payload server ID and exact
tool name to match them. No
`stable_logical_segment` or caller-facing encoder may turn invalid input into a
valid source. Fixtures cover every ASC-001 source scope, multiple exact tool
names on one configured server, distinct configured keys, parser mismatch
rejection, and component IDs before and after runner observation.
`from_json_str` and `from_json_slice` reject raw input above
`RUNTIME_FINGERPRINT_MAX_ENVELOPE_BYTES = 2_097_152` before Serde allocation,
then enforce the base and complete-locator limits while parsing. Exact raw
envelope size can be reached with trailing JSON whitespace; byte 2,097,153
fails typed before decoding.
This implements B-003 and B-011.

Runtime version-probe authorization is a private conjunction of source and
opened-target policy. `Repository` source maps to `IdentityOnly`; resolution,
handle inspection, and hashing may run, but the payload records
`version_probe/probe_not_authorized` with
`configuration_source_repository`. Registered `Observation(...)` helpers
may derive retained identity/hash/authorization evidence, but no
`InitialTarget` or `RetryTarget` is created and no target, loader, or
interpreter instruction runs. `UserGlobal`,
`Admin`, `System`, `Runtime`, and genuine `Runner` pass only the source half.
After opening the target, the producer resolves the final handle path and
compares it, with platform-correct component and case semantics, against every
canonical root in a typed `ValidatedRepositoryBoundarySet` derived from the
declared project repository and linked worktree roots. A target inside any
boundary maps to `IdentityOnly` with `resolved_target_repository`. Missing,
incomplete, renamed, or ambiguous final-handle/boundary evidence records
`target_authorization_unavailable` with `BoundaryUnprovable`. On supported
Linux, the same authorization step requires exact handle `st_nlink == 1`;
zero records `target_authorization_unavailable` with `UnlinkedTarget`, a count
greater than one records `MultipleHardLinks`, and an unavailable count records
the honest `LinkCountUnprovable` reason. v0.1 deliberately
rejects otherwise legitimate multiply linked binaries because it cannot prove
every alias lies outside the repository boundaries. This closes hard-link
ambiguity only and does not claim to enumerate bind-mount or other namespace
aliases. Only a single-link target proven outside every boundary becomes
`VersionProbeEligible`. Raw roots and final paths are never serialized. There
is no caller boolean, string trust level, path label, or builder that can
promote either identity-only result. This policy does not reinterpret ASC-001
observation or trust metadata.

Tool name and description are copied as exact UTF-8 strings. The producer does
not call `trim`, `split_whitespace`, Unicode normalization, case conversion, or
punctuation rewriting. `None`, `Some("")`, spaces, tabs, and newlines remain
distinct serialized facts and digest inputs, as required by B-012. Optional
annotations enter only through `McpToolAnnotations::from_json_str` or
`from_json_slice`; there is no typed-map, `serde_json::Value`, or generic
serializable constructor and annotation hints never populate ASC capabilities.


Runtime command resolution, retained-handle observation, and exact-pidfd
supervision are normative in `runtime-observation.md` and
`runtime-supervision.md`.

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

Present `PATH` and `CLAUDE_CONFIG_DIR` evidence uses the same exact OS-unit
encoding helper as B-004 but separate frozen domains:

```text
SHA-256(domain || platform_tag || unit_count_be || exact_units)
```

The domains are exactly
`b"harness_runtime_environment_path_v0_1\0"` and
`b"harness_runtime_environment_claude_config_dir_v0_1\0"`.
`platform_tag` is `b"unix\0"` or `b"windows\0"`; the count is fixed-width
`u64` big-endian. Unix counts raw `OsStr` bytes and appends them unchanged.
Windows counts original UTF-16 code units and appends each little-endian `u16`.
There is no UTF-8 conversion, normalization, case folding, separator rewrite,
or path parsing before hashing. `Unset` has no digest; a present empty value
hashes the zero-unit encoding and remains distinct.

No other ordinary key enters evidence or the version child. There is no public
`RuntimeEnvironmentDeclaration`, classification builder, automatic public
fallback, or caller-set exposure flag. The closed table is the only source of
`unset`, `redacted`, and digest facts.

The producer counts at most 1,025 observation-environment entries, then
measures at most 1,025 exact OS units per environment key in input order. Only
after that collection passes does it count at most 1,025 setup-secret names and
measure their exact OS units in input order. Counting and measurement do not
copy or canonicalize a name. Unix measures raw key bytes before UTF-8/name
shape validation and retains exact case-sensitive UTF-8 names. Windows measures
original UTF-16 units, then accepts only ASCII names, canonicalizes them to
uppercase for comparison, rejects canonical collisions such as `Path` plus
`PATH`, and serializes the policy spelling from the table. A non-ASCII Windows
name fails typed because the producer cannot reproduce the OS's Unicode
case-insensitive comparison without an authorized platform primitive.

`codex.cloud.setup_secret_env` is a separate exclusion set, not a source of
policy entries. Its names pass through the same platform canonicalizer, and
matching keys are removed from evidence and child input before policy lookup.
Thus setup-secret exclusion overrides even a listed policy key. PATH cannot be
represented a second time as an ordinary entry. The producer does not inspect,
copy, bound, or hash the value of an undeclared or setup-secret-excluded key.
Only an exclusion-surviving policy-selected `PATH` or `CLAUDE_CONFIG_DIR` value
is read and checked against the 65,536-unit value limit. An excluded over-limit
`CLAUDE_CONFIG_DIR` is absent without a limit error; an excluded over-limit
`PATH` becomes `Unset`, so a bare Unix command later returns the existing
`path_unusable` outcome. Raw values and undeclared keys never enter the
envelope or probe `envp`, and the producer never reads them for evidence. v0.1
does not claim that the authorized target is unable to open readable same-UID
host files or process state; such a claim requires a separate filesystem and
process-isolation design plus security approval. These rules implement B-005
and B-010.

## Bounded MCP Annotations and Schema Canonicalization

Optional `McpToolAnnotations` exposes only `from_json_str` and
`from_json_slice`. It uses the same duplicate-detecting raw JSON visitor and
raw-number preservation as schemas, requires an object root, and traverses the
whole value as `InstanceData`: object keys sort, but every array preserves
order and keyword-shaped keys never enter schema context. Absence and `{}` are
distinct. The payload therefore preserves standard boolean hints, title, and
vendor values without treating any annotation as an ASC capability. Its fixed
limits are raw bytes 65,536, canonical bytes 49,152, depth 32, nodes 4,096,
cumulative decoded-string bytes 32,768, and entries per container 1,024.
Every exact limit and limit-plus-one has a typed `McpContractLimitKind`
fixture. There is no annotations constructor from `serde_json::Value`, a
typed map, or a generic serializable value.

The required `McpInputSchema` and optional `McpOutputSchema` share one private
`McpToolSchema` representation and expose only subject-specific
`from_json_str` and `from_json_slice` constructors. Both start from raw JSON
and use a duplicate-detecting Serde visitor rather than first decoding to
`serde_json::Value`, because the latter can overwrite an earlier duplicate key.
With the explicitly enabled `serde_json/raw_value` feature, each object member
value and array element is first borrowed as `&RawValue`; the private visitor
recurses over `RawValue::get()` while retaining the source slice for a number
leaf. This uses serde_json's validated raw-value scanner, not a handwritten
JSON lexer, and preserves `1`, `1.0`, `1e0`, and arbitrarily long valid
in-bound number tokens as distinct lexemes. After parse and before
canonicalization, the root must be an
object; root boolean, array, string, number, and null values return a typed
`RootNotObject` contract error and emit no digest. Boolean schemas remain legal
only in schema-valued child positions. Malformed JSON and
duplicate-object-key errors remain typed and occur before canonicalization or
digesting. Invalid JSON-number spellings fail syntax validation. The visitor
retains each validated raw JSON number token for the exact B-014 encoding
instead of round-tripping it through `i64`, `u64`, or `f64`. There is no public
`from_serializable`, `serde_json::Value`, or typed-map evidence constructor:
after ordinary decoding, original duplicate-key absence cannot be attested.

The parser and canonicalizer enforce fixed v0.1 resource limits:

| Resource | Inclusive maximum |
| --- | ---: |
| configured MCP server stable-key UTF-8 bytes | 1,024 |
| tool-name UTF-8 bytes | 1,024 |
| description UTF-8 bytes | 65,536 |
| annotations raw bytes | 65,536 |
| annotations nesting depth | 32 |
| annotations JSON nodes | 4,096 |
| annotations decoded string bytes | 32,768 |
| entries in one annotations object or array | 1,024 |
| canonical annotations bytes | 49,152 |
| raw schema bytes | 1,048,576 |
| schema nesting depth | 64 |
| total JSON nodes | 65,536 |
| cumulative decoded string bytes | 524,288 |
| entries in one object or array | 4,096 |
| canonical schema bytes | 786,432 |

The stable-key check occurs before hex expansion. Each present annotations,
input-schema, and output-schema value independently receives its table limits.
Raw bytes are checked before parse. Depth is the count of JSON value nodes on a
root-to-value path with root depth 1. Every value—including root, object member
value, and array element—is one node; object keys are not nodes. Cumulative
decoded-string bytes include each object key and string value after unescaping,
once per occurrence. A container's entries are its direct members/elements.
On entering a value the visitor checks depth, then increments/checks nodes;
before accepting a member/element it increments/checks that container; after
decoding a key or string it adds/checks UTF-8 bytes. Duplicate detection remains
part of this same deterministic source-order visit. Canonical-byte charging
runs only after successful parse/structural budgets and before allocating each
output fragment or set-sort key.

Independent schema limit vectors are frozen as follows:

- `D1 = {}` and `D(n+1) = {"x":D(n)}`: `D64` is accepted and `D65` is depth
  limit-plus-one.
- `{"x":[A1,...,A15,B]}`, where every `Ai` is an array of 4,096 `null`
  values and `B` contains 4,078 `null` values, has exactly 65,536 nodes; adding
  one `null` to `B` has 65,537.
- `{"x":"<524287 ASCII a bytes>"}` has exactly 524,288 decoded string bytes
  including key `x`; one more value byte is limit-plus-one.
- one root object with exactly 4,096 distinct `k<decimal>` members is accepted;
  member 4,097 is per-container limit-plus-one.
- compact `{"x":<one digit 1 followed by 786425 zeroes>}` is exactly 786,432
  canonical bytes; one more zero is canonical limit-plus-one while raw bytes
  remain below their maximum.
- `{}` followed by exactly 1,048,574 ASCII spaces is exactly 1,048,576 raw
  bytes; one more space is raw limit-plus-one.

Exact limits are legal; limit-plus-one returns the specific closed
`McpContractLimitKind` and emits no digest or envelope. Depth 64 keeps recursive
schema traversal below the fixed safe bound. Borrowed `RawValue` recursion
requires only the explicitly enabled existing feature and no new parser,
package, or lockfile entry.

Before entering the private state machine, the duplicate-aware root parser
selects one closed `McpSchemaDialect`: absent `$schema` and exact
`https://json-schema.org/draft/2020-12/schema` select `Draft202012`; exact
`http://json-schema.org/draft-07/schema#` selects `Draft07`. Any other
root value, a non-string root value, or `$schema` in a nested `Schema` context
returns `UnsupportedSchemaDialect`. The exact root member remains in canonical
JSON. v0.1 does not fetch a metaschema or permit an embedded dialect switch.

The private state machine carries that dialect through these contexts:

- `Schema(dialect)`: an object whose recognized keywords come only from that
  dialect;
- `SchemaMap(dialect)`: schema-valued map children;
- `StringSetMap(dialect)`: dialect-recognized property-dependency string sets;
- `LegacyDependenciesMap(Draft07)`: every dependency value is an
  object/boolean schema or an array of strings canonicalized as a set;
- `SchemaArrayOrdered(dialect)`: recognized tuple/prefix children traverse as
  schemas but preserve array order;
- `SchemaArraySet(dialect)`: `allOf`, `anyOf`, and `oneOf` traverse as schemas
  and sort canonical bytes; and
- `InstanceData`: object members sort but all arrays preserve order recursively.

In both dialects, `required`, `type`, and `enum` arrays are canonical sets;
`allOf`, `anyOf`, and `oneOf` enter `SchemaArraySet`. `enum` elements,
`default`, `const`, `examples`, and `example` enter `InstanceData`.
`properties` and `patternProperties` are schema maps in both dialects.
The shared single-schema keywords `not`, `if`, `then`, `else`, `contains`,
`propertyNames`, and `additionalProperties` always enter `Schema(dialect)` in
both dialects, even when an adjacent activating keyword is absent. Each accepts
only an object or boolean schema; array, string, number, and null return
`MalformedSingleSchemaKeyword { dialect, keyword }`. The closed keyword enum
contains exactly `Not`, `If`, `Then`, `Else`, `Contains`, `PropertyNames`,
`AdditionalProperties`, `Items`, `AdditionalItems`, `ContentSchema`,
`UnevaluatedItems`, and `UnevaluatedProperties`. Consequently a nested
`$schema` inside any recognized single-schema position is rejected by the
existing nested-dialect rule rather than hidden as instance data.

For `Draft202012`, `$defs` and `dependentSchemas` are schema maps;
`dependentRequired` enters `StringSetMap`; `prefixItems` enters
`SchemaArrayOrdered`; `items` accepts only an object or boolean schema; and
`contentSchema`, `unevaluatedItems`, and `unevaluatedProperties` are
single-schema locations. `definitions`, `dependencies`, and
`additionalItems` are extension instance data. Array-form `items` is a typed
malformed standard keyword rather than legacy tuple syntax.

For `Draft07`, `definitions` is a schema map, `dependencies` enters
`LegacyDependenciesMap`, array-form `items` enters `SchemaArrayOrdered`, and
object/boolean `items` enters `Schema(Draft07)`. `additionalItems` always enters
`Schema(Draft07)` whether or not array-form `items` is present; its validation
effect is separate from its standard schema-valued grammar. `$defs`,
`dependentSchemas`, `dependentRequired`,
`prefixItems`, `contentSchema`, `unevaluatedItems`, and
`unevaluatedProperties` are extension instance data. Unknown/vendor-extension
values are `InstanceData` in either dialect. Object members sort in all
contexts, but arrays sort only in the recognized set contexts for the selected
dialect.

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

## Authorized Implementation Surface

Only these paths are authorized:

1. `crates/harness-core/Cargo.toml` (enable `serde_json/raw_value` only)
2. `crates/harness-core/src/stack/mod.rs`
3. `crates/harness-core/src/stack/fingerprint.rs`
4. `crates/harness-core/src/stack/fingerprint/model.rs`
5. `crates/harness-core/src/stack/fingerprint/schema.rs`
6. `crates/harness-core/src/stack/fingerprint/tests.rs`
7. `crates/harness-agents/Cargo.toml` (direct existing workspace `libc` only)
8. `crates/harness-agents/src/lib.rs`
9. `crates/harness-agents/src/runtime_fingerprint.rs`
10. `crates/harness-agents/src/runtime_fingerprint/environment.rs`
11. `crates/harness-agents/src/runtime_fingerprint/executable.rs`
12. `crates/harness-agents/src/runtime_fingerprint/probe.rs`
13. `crates/harness-agents/src/runtime_fingerprint/tests.rs`
14. `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs`
    (`#[cfg(test)]` exhaustive mapping contract only)

Moving the two existing inline test modules into their listed test files is
part of this scope. The agents manifest may add only `libc = { workspace =
true }`; the core manifest may change only its existing `serde_json` dependency
to `{ workspace = true, features = ["raw_value"] }`. There is no new
crate/version or lockfile change. No other manifest,
database, configuration, adapter, spawn contract, workflow model, CLI, HTTP,
prompt, snapshot, or high-context file change is authorized. Any production
server import or call site requires an ASC-005 consumer specification rather
than an amendment here.

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
cargo audit
git diff --check
```

The changed-file audit must equal the fourteen-path manifest. `Cargo.lock` must
be unchanged and `cargo tree -p harness-agents -i libc` must show the pinned
workspace dependency. `cargo tree -e features -p harness-core` must show the
direct `serde_json/raw_value` feature. A call-site audit
must show that production uses of the new APIs remain confined to their
defining modules; test uses do not count as consumers. File-length checks must
show every Rust file below 800 lines. The sandbox parity gate and Linux
retained-directory/retained-executable path require mandatory human security
review for exact `SandboxSpec` passthrough equivalence, observation-process
fixed-frame/`SCM_RIGHTS` protocol, pidfd ownership and revalidation,
bounded pre-ready inherited-fd transient, post-ready descriptor
allowlists/foreign-fd isolation/start-gate ordering and direct-child rollback,
capability-child validating/consuming `waitid(P_PIDFD)` plus bootstrap
exact-PID fallback, global owner-permit lifetime, owner/helper/child
descriptor ledgers, bounded launch/environment/setup-secret counting before
hashing/splitting/joining, allocation-free post-fork work, descriptor ownership,
`fchdir` ordering and error staging, `FD_CLOEXEC` script rejection, ptrace-stop ordering,
stopped-image identity/hash validation under kernel write denial,
W+X/executable-stack rejection, post-exec syscall-stop denial of process
creation, image execution, executable mappings, and existing executable-image
mutation,
post-capability registered-pidfd-only signalling and reap ordering, legal
signal-delivery reinjection and illegal-state rejection,
argument/environment pointers, NUL validation, error
propagation, proof that authorization cannot fall back to a pathname, and proof
that `ENOEXEC` never starts a shell. Because the repository
no longer contains the historical SpecRail
checker, verification must not claim to run a removed script; structural
review of the manifest and B-001 through B-016 coverage is the current spec
check.

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
- Using a Unix process group, PGID, or `/proc` membership as completion
  evidence is rejected. v0.1 proves only that its exact pidfd registry is empty
  and the guarded target executed no process-creation syscall.
- Running an unrestricted probe for a restricted or path-narrowed adapter
  launch is rejected. Reusing the current sandbox wrapper would replace the
  retained-handle target with a wrapper/path launch, so v0.1 accepts only the
  exact passthrough sandbox state and otherwise fails before host observation.
- Executing repository-owned code merely to obtain `--version` is rejected:
  repository sources and any resolved repository/worktree target produce
  identity-only evidence with `probe_not_authorized` until a separately
  specified hardened sandbox exists.
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

The residual source-path history gap between checkpoints is explicit and is
not promoted to path attestation. Executed-byte attribution instead requires
`FD_CLOEXEC` retained-handle `execveat(AT_EMPTY_PATH)`, a verified
`PTRACE_EVENT_EXEC` before the first instruction, stopped-image strong
identity, and a matching handle hash while kernel write denial is active;
without every condition, the child is killed before resume. Platform
pidfd, ptrace, descriptor-isolation, and file-identity capabilities differ;
unsupported guarantees must yield the applicable typed failure rather than a
warning-only fallback.
