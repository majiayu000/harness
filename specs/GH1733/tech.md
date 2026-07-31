# Tech Spec

## Linked Issue

GH-1733

## Product Spec

See `specs/GH1733/product.md`.

<!-- specrail-planned-changes
{"issue":1733,"complete":true,"paths":["crates/harness-agents/Cargo.toml","crates/harness-agents/src/lib.rs","crates/harness-agents/src/runtime_fingerprint.rs","crates/harness-agents/src/runtime_fingerprint/environment.rs","crates/harness-agents/src/runtime_fingerprint/executable.rs","crates/harness-agents/src/runtime_fingerprint/probe.rs","crates/harness-agents/src/runtime_fingerprint/tests.rs","crates/harness-core/src/stack/fingerprint.rs","crates/harness-core/src/stack/fingerprint/model.rs","crates/harness-core/src/stack/fingerprint/schema.rs","crates/harness-core/src/stack/fingerprint/tests.rs","crates/harness-core/src/stack/mod.rs","crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012","B-013","B-014","B-015","B-016"]}
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
the already pinned workspace `libc` solely for Linux no-shell process-group,
pidfd, ptrace exec-stop, and `execveat(AT_EMPTY_PATH)` primitives. This adds no
package/version and must not change `Cargo.lock`.

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
the latter computes ASC-001 integrity internally, and there is no bare-digest
setter. `ConfiguredRuntimeExecutable` asks the core typed binding to derive the
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
before hex expansion or locator allocation; byte 1,025 fails typed. There is no
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
This implements B-003 and B-011.

Runtime version-probe authorization is a private conjunction of source and
opened-target policy. `Repository` source maps to `IdentityOnly`; resolution,
handle inspection, and hashing may run, but the payload records
`version_probe/probe_not_authorized` with
`configuration_source_repository`, and no process is created. `UserGlobal`,
`Admin`, `System`, `Runtime`, and genuine `Runner` pass only the source half.
After opening the target, the producer resolves the final handle path and
compares it, with platform-correct component and case semantics, against every
canonical root in a typed `ValidatedRepositoryBoundarySet` derived from the
declared project repository and linked worktree roots. A target inside any
boundary maps to `IdentityOnly` with `resolved_target_repository`. Missing,
incomplete, renamed, or ambiguous final-handle/boundary evidence records
`target_authorization_unavailable`. Only a target proven outside every
boundary becomes `VersionProbeEligible`. Raw roots and final paths are never
serialized. There is no caller boolean, string trust level, path label, or
builder that can promote either identity-only result. This policy does not
reinterpret ASC-001 observation or trust metadata.

Tool name and description are copied as exact UTF-8 strings. The producer does
not call `trim`, `split_whitespace`, Unicode normalization, case conversion, or
punctuation rewriting. `None`, `Some("")`, spaces, tabs, and newlines remain
distinct serialized facts and digest inputs, as required by B-012. Optional
annotations enter only through `McpToolAnnotations::from_json_str` or
`from_json_slice`; there is no typed-map, `serde_json::Value`, or generic
serializable constructor and annotation hints never populate ASC capabilities.

## Single-Command PATH Resolution

`executable.rs` resolves one configured command without a shell. Windows and
Unix each freeze the explicit v0.1 algorithm below independently of the
compiler used to build Harness. The input carries a typed
`RuntimeLaunchContext`: platform, configured child working directory,
sanitized child `PATH`, and every platform search base that the resolver needs
but must not infer. It also carries the validated repository boundary set used
only for B-007 target authorization; an absent or incomplete set can still
produce identity evidence but can never authorize process creation.

The payload records a closed `RuntimeCommandForm` with exactly `UnixBare`,
`UnixAbsolute`, `UnixQualified`, `WindowsBare`, `WindowsAbsolute`, or
`WindowsQualified`. The producer derives it from the original configured
`OsStr`; the parser uses it to validate search versus configured-path outcomes
without serializing the command.

Before resolution, the producer computes `configured_command_digest` from a
domain-separated exact OS-string representation:

```text
"harness_runtime_configured_command_v0_1\0"
  || platform_tag
  || unit_count_be
  || exact_units
```

`platform_tag` is exactly `b"unix\0"` or `b"windows\0"`.
`unit_count_be` is an unsigned fixed-width `u64` in big-endian order. Unix
counts and appends `OsStrExt::as_bytes()` unchanged. Windows counts original
`encode_wide()` code units and appends each as little-endian `u16`. No UTF-8 conversion,
case-folding, slash normalization, dot-segment folding, absolutization, or
symlink resolution occurs. Empty or NUL-containing commands fail typed before
hashing. The digest is a payload fact and enters `fingerprint_digest`; the raw
command is never serialized and the value never becomes ASC-001 integrity.

The configured child working-directory spelling uses the same helper with
domain
`b"harness_runtime_working_directory_v0_1\0"` and enters the payload as
`working_directory_digest`. After the isolation and sandbox gates, Linux v0.1
first proves `pidfd_open`, `pidfd_send_signal`, `/proc` membership enumeration,
and the kill-isolated observation protocol are available, reserves the
runtime-independent owner, and starts the active deadline. macOS, other Unix,
and Windows return typed no-envelope producer error
`ContainmentUnavailable(UnsupportedPlatform)` before cwd observation. On
supported Linux, an observation subprocess opens the directory once with
`O_RDONLY | O_DIRECTORY | O_CLOEXEC`, returns the descriptor over the private
fixed-frame socket using `SCM_RIGHTS`, and the parent retains it for every later
observation and target helper. Authoritative directory handle metadata is
required. It records

```text
SHA-256(
  "harness_runtime_working_directory_identity_v0_1\0"
  || u64be(st_dev)
  || u64be(st_ino)
)
```

as `working_directory_identity_digest`. Open or metadata failure is typed
producer-input `WorkingDirectoryUnavailable` and emits no envelope. Raw path,
device, and inode never enter evidence.

Independent vectors are exact: Unix working-directory bytes `/x` hash to
`bdc1de448a5df96390bcc54bf757c96abf628c534baef27bdeba60c5350ebaf6`;
Windows UTF-16 units for `C:\X` hash to
`90e7e9eb468b08a8b8b5161fb2211bcba076a30439db72f7d6761d6398372085`;
and Unix `st_dev = 1`, `st_ino = 2` hash to
`0980191ed8a4adfd1d3a83af85fb72a46b9aae6ff342d53517995d161ee7f4f9`.

The first operations are the isolation and sandbox gates above. Their ordering
is a tested privacy and evidence boundary, not merely an unsupported branch
later in the probe.

On Unix:

- an absolute path becomes the sole
  `RuntimeResolvedCandidate::Absolute` reference;
- a qualified relative path becomes a
  `RuntimeResolvedCandidate::WorkingDirectoryRelative` reference;
- a bare name traverses sanitized `PATH` in order, with empty and relative
  entries producing working-directory-relative references and absolute entries
  producing absolute references;
- every preliminary observation and authoritative open of a
  working-directory-relative reference uses `fstatat`/`openat` (or an exact
  handle-relative equivalent) against the one retained working-directory
  descriptor; no access reconstructs an absolute pathname;
- a bare name with sanitized child `PATH = Unset` is `path_unusable` before
  candidate observation; v0.1 does not guess libc's platform default, while
  absolute and qualified commands remain representable;
- for `UnixBare`, missing, non-regular, and mode-ineligible same-basename
  candidates advance without process creation;
- for `UnixAbsolute` or `UnixQualified`, an exact missing result is terminal
  `path_not_found`, while an existing non-regular or mode-ineligible candidate
  is terminal and requires `identity/not_regular_file` or
  `identity/not_executable`;
- no more than 64 sanitized PATH entries are observed; reaching entry 65
  before a terminal outcome emits `candidate_limit_exceeded`;
- exact `ENOENT` or `ENOTDIR` from authoritative open is `Absent`, just like
  the preliminary observation: a bare search advances and an
  absolute/qualified sole candidate ends as `path_not_found`;
- every other failure to open an otherwise statically eligible candidate stops
  with `identity/open_failed`, because skipping an unreadable execute-only
  candidate could select a different executable than the adapter;
- after handle inspection and the pre-spawn checkpoint, `probe.rs` keeps
  `FD_CLOEXEC` on the retained authorized handle and uses direct
  `execveat(..., AT_EMPTY_PATH)` from the existing workspace `libc` dependency
  under a `PTRACE_O_TRACEEXEC` parent trace, with fixed arguments and no shell;
  interpreter-script exec fails before interpreter execution because the script
  descriptor closes on exec; a successful native exec stops at
  `PTRACE_EVENT_EXEC` before its first instruction, and only matching stopped-
  image strong identity plus a retained-handle re-hash under kernel write denial
  allows resume; `argv[0]` retains the exact original configured command
  `OsStr` units used by the adapter;
- path-based `execve` after authorization is forbidden; a Linux platform on
  which process-group supervision is otherwise available but no verified
  traced retained-handle `execveat(AT_EMPTY_PATH)` primitive exists records
  `handle_execution_unavailable` before anchor or target creation and never
  falls back to the pathname;
- only exact `EACCES` from that call advances to the next same-basename
  candidate;
- exact first `ETXTBSY` waits the adapter's fixed 150 milliseconds, repeats
  opened-target authorization plus the full retained-handle hash and path
  strong-identity checkpoint, and retries the same retained authorized handle
  once; the retained candidate reference and its lexical digest remain
  resolution evidence only;
  identity change stops without retry, while second `ETXTBSY`, `ENOEXEC`, and
  every other retry/error are terminal `spawn_failed`;
- absolute and qualified commands may use that one same-candidate `ETXTBSY`
  retry but never search or fallback to another path; and
- the first successful exec becomes the selected executable.

The implementation must not call `execvp`: POSIX `ENOEXEC` fallback may invoke
`/bin/sh`, which violates the no-shell boundary. Every observed Unix candidate
adds one ordered `RuntimeResolutionAttempt`; absolute and qualified commands
have exactly one, while a bare name has at most 64. Its digest is:

```text
SHA-256(
  "harness_runtime_resolution_candidate_v0_1\0"
  || platform_tag
  || unit_count_be
  || exact_units
)
```

The tags, counts, and exact units are encoded identically to
`configured_command_digest`. Each attempt also carries a closed
`RuntimeExecSequence`: `None`, `Single`, or
`EtxtbsyThenCheckpointAfter150Ms`. The closed
`RuntimeResolutionAttemptOutcome` is exactly `Absent`, `NotRegular`,
`NotExecutable`, `InspectionFailed`, `InspectionTarget`,
`AuthorizationUnavailable`, `InterpreterAuthorizationUnavailable`,
`HandleExecutionUnavailable`, `RetryNotAuthorized`,
`RetryAuthorizationUnavailable`, `SupervisionSetupFailed`, `ExecEacces`,
`ExecVerificationFailed`, `ExecFailed`, or `ExecStarted`.
`InterpreterAuthorizationUnavailable` is terminal, requires
`version_probe/interpreter_authorization_unavailable`, and uses
`RuntimeExecSequence::None` when the bounded pre-anchor classifier observes a
script, dynamic or malformed ELF, wrong-architecture ELF, or any other
unsupported format, or `Single` / `EtxtbsyThenCheckpointAfter150Ms` for exact
exec-time `ENOENT`; every form proves no target/loader/interpreter instruction
ran and any setup helper was reaped. `ExecVerificationFailed` requires
`identity/identity_changed`, uses `Single` or the retry sequence, and proves the
exec-stopped child was killed/reaped without resume.
`SupervisionSetupFailed` is
terminal, requires
the matching version-probe failure, uses `RuntimeExecSequence::None` for initial
anchor/target setup or `EtxtbsyThenCheckpointAfter150Ms` for the second helper
after an exact first `ETXTBSY`, and proves every created helper was reaped before
the affected target handle-exec call. Attempts preserve PATH order and duplicates; they
are never sorted.

For an absolute candidate, digest units are its exact absolute spelling. For a
working-directory-relative candidate, digest units are the lexical join of the
configured working-directory spelling and exact relative reference, without
normalization. That lexical value is evidence only and is never passed to an
access API; `working_directory_identity_digest` plus handle-relative
`fstatat`/`openat` binds the actual directory used.

The parser enforces a finite-state contract together with
`RuntimeCommandForm`. Under `UnixBare`, `Absent`, `NotRegular`, and
`NotExecutable` are skips and may precede `ExecEacces` or one terminal outcome.
Under `UnixAbsolute` or `UnixQualified`, exactly one attempt is required:
`Absent` terminates as `path_not_found`, while `NotRegular` and
`NotExecutable` are terminal and require their matching identity failure.
`InspectionFailed` requires the matching
identity failure. `InspectionTarget` is permitted only with
`probe_not_authorized` and one closed configuration-source or resolved-target
repository reason, and forbids all exec outcomes. `AuthorizationUnavailable`
requires exactly `target_authorization_unavailable`, is terminal, and forbids
exec, fallback, or selected identity. `InterpreterAuthorizationUnavailable`
requires exactly the matching failure and its sequence distinguishes
pre-observed shebang from exact exec-time `ENOENT`; both forbid fallback and
selected identity. `ExecVerificationFailed` requires `identity_changed`,
forbids resume/fallback/selected identity, and proves the stopped child was
reaped.
`HandleExecutionUnavailable` requires
exactly `handle_execution_unavailable`, is terminal, uses `None`, and forbids
anchor/target creation, path fallback, and selected identity. `ExecFailed`
requires `spawn_failed`; `ExecStarted` forbids a spawn failure and is the only
outcome that creates a selected/executed identity. A sequence containing only
skips yields `path_not_found`; one ending in `ExecEacces` with no final identity
yields `bare_eacces_exhausted`; reaching the bound without a terminal outcome
yields `candidate_limit_exceeded` with exactly 64 attempts. Outcomes after a
terminal, multiple terminals, wrong source/failure pairs, or more than 64
entries fail parsing. A Windows command form forbids Unix attempt evidence.

`RuntimeExecSequence::None` is required for skips, inspection-only,
pre-observed unsupported-format interpreter-authorization-unavailable,
handle-execution-unavailable, initial authorization-unavailable, and initial
supervision-setup-failed outcomes. `Single` is required for an ordinary exec
outcome, exact exec-time interpreter failure, or exec-verification failure.
`EtxtbsyThenCheckpointAfter150Ms` is legal only after the first direct
exec returned raw errno `ETXTBSY`; it requires the 150-millisecond monotonic
delay and the repeated authorization/hash/path-identity checkpoint. If that
checkpoint changes authorization, `RetryNotAuthorized` requires
`probe_not_authorized` with exact `ResolvedTargetRepository` reason, while
`RetryAuthorizationUnavailable` requires
`target_authorization_unavailable`; either forbids the second helper, fallback,
and selected identity. `InspectionFailed` with `identity_changed` likewise
forbids the second helper. `SupervisionSetupFailed` is also legal in this
sequence when the second helper is reaped after group join fails and before
target handle exec; it cannot fall back. Only `ExecEacces` for a bare name,
`InterpreterAuthorizationUnavailable`, `ExecVerificationFailed`, `ExecFailed`,
or `ExecStarted` proves that the second target exec was attempted.
A second `ETXTBSY` is
`ExecFailed`. Absolute and qualified commands reject `ExecEacces` and represent
terminal `EACCES` as `ExecFailed`. Cancellation during the delay emits no
envelope and starts no child. No other errno, retry count, or delay is accepted.

Failed candidates contribute no final executable identity. Repository-owned
bare commands stop after the first statically eligible successful inspection,
record that handle as an `inspection_target`, emit `probe_not_authorized`, and
never enter pre-exec or fallback. The same stop applies to any otherwise
eligible source whose opened target is inside a repository/worktree boundary.
An earlier open or identity failure wins and no authorization failure is
appended. Exact `ENOENT`/`ENOTDIR` has precedence over `open_failed` as defined
above. Unavailable target authorization also stops without exec or fallback.
After target authorization, the observation helper runs a bounded executable
classifier over the retained handle. It requires exact ELF magic, the current
architecture's frozen machine/class/endianness tuple, `EI_VERSION` and
`e_version` equal to `EV_CURRENT`, exact ELF64 `e_ehsize = 64` and
`e_phentsize = 56`, `ET_EXEC` or `ET_DYN`, and `e_phnum` in
`1..PN_XNUM` (extended program-header counts are unsupported). v0.1 supports
only Linux `x86_64` (`EM_X86_64`, `ELFCLASS64`, little-endian) and Linux
`aarch64` (`EM_AARCH64`, `ELFCLASS64`, little-endian); another build target
fails the pre-observation capability gate. Checked arithmetic must prove
`e_phoff + e_phnum * e_phentsize` is within the retained file, and every
program header is scanned to reject `PT_INTERP`. Other header fields are not
authorization signals; a later kernel load error remains `spawn_failed`.
Exact `#!`, dynamic or structurally malformed ELF, wrong-machine ELF, and every
non-ELF/binfmt format emit
`InterpreterAuthorizationUnavailable` before anchor or target creation; no
header bytes, interpreter path, or raw prefix are serialized. If accepted
bytes become a script after this check,
`FD_CLOEXEC` retained-handle `execveat(AT_EMPTY_PATH)` fails before interpreter
execution; exact exec-time `ENOENT` is the same terminal interpreter failure,
not PATH fallback. This is intentionally conservative because script execution
or dynamic loading delegates to another executable that this packet does not
authorize. Fault-injection tests freeze these paths without a `noexec`
filesystem.

All `CString` argument and environment storage and pointer arrays are built and
NUL-validated in the parent. The audited Linux pre-exec closure receives the
retained working-directory and `FD_CLOEXEC` executable descriptors. It uses
only async-signal-safe `ptrace(PTRACE_TRACEME)`, `raise(SIGSTOP)`, `setpgid`,
`fchdir`, descriptor close, and `execveat(AT_EMPTY_PATH)`; it never allocates,
logs, locks,
resolves, or reopens a pathname after fork. It first joins the anchored group
and enters the exact retained working directory, then requests tracing and
stops. Only after the parent installs `PTRACE_O_TRACEEXEC` does it close
unrelated descriptors and call `execveat`. Its stage-tagged error pipe
distinguishes group-join, working-directory-entry, trace-setup, and target
handle-exec failure. A failed `setpgid`, `fchdir`, ptrace request, stop, or
option install produces
`supervision_setup_failed`, and the parent reaps that helper before emitting
evidence; its closed `RuntimeSupervisionSetupStage` is `GroupJoin`,
`WorkingDirectoryEnter`, or `TraceSetup` (`AnchorSetup` is reserved for anchor
failure). `TraceSetup` covers `PTRACE_TRACEME`, the initial stop, and parent
`PTRACE_O_TRACEEXEC` installation, all before target exec.
Successful handle exec never returns. A failed target call returns its
captured errno through the distinct exec channel, so only exact target
`EACCES` reaches the fallback branch, exact `ENOENT` maps to terminal
interpreter authorization unavailable, and a setup errno or `ENOEXEC` cannot
reach a fallback or shell execution path.

On Windows, v0.1 independently freezes this bare-name resolver: explicit child
`PATH`, current executable directory, system directory, Windows directory,
then parent `PATH`, with only `.exe` completion. The algorithm and fixtures are
not derived from the compiler that happens to build Harness. `PATHEXT` is not
consulted. Explicit `.bat` and `.cmd` programs are `path_unusable` because the
adapter's batch handling invokes a command interpreter and violates the
no-shell boundary. Every explicit non-`.exe` extension is likewise
`path_unusable` in v0.1; supporting another extension requires a later closed
grammar revision.
The child `current_dir` is not treated as the parent search base. A qualified
relative program, relative/empty search entry, or unavailable special directory
is accepted only when its actual base is explicit and stable in the launch
context; otherwise resolution records `path_unusable`. A differential
conformance test runs the current adapter `Command` against fixed fixtures and
fails if it diverges from the frozen resolver; it never updates expected
behavior from the running compiler.

On every platform, quotes, spaces, pipes, substitutions, and redirections are
literal path characters. Resolution inspects only the configured basename,
never runs `sh`, `which`, or a package manager. Windows, absolute, and qualified
commands have one absolute selected candidate and no fallback. Unix bare-name
fallback is limited to the ordered, inspected, same-basename `EACCES` algorithm
above; it never changes arguments or executes a search helper.

The payload carries a closed `WindowsResolutionContextEvidence` with exactly
four optional fields:
`current_executable_dir_digest`, `system_dir_digest`,
`windows_dir_digest`, and `parent_path_digest`. Each present value uses:

```text
SHA-256(domain || b"windows\0" || u64be(utf16_unit_count) || utf16le_units)
```

The domains are exactly:

```text
b"harness_runtime_windows_search_current_executable_dir_v0_1\0"
b"harness_runtime_windows_search_system_dir_v0_1\0"
b"harness_runtime_windows_search_windows_dir_v0_1\0"
b"harness_runtime_windows_search_parent_path_v0_1\0"
```

Absent is a distinct typed state with no digest; present empty hashes the
zero-unit framing. For the independent original UTF-16 units of `C:\X`
(`0043 003a 005c 0058`), the four digests in field order are exactly:

```text
4864a078702061a4fd859437dcadfce7519d755e47de020039dd4473d3651e7e
cc203ab9fd082171309ae3c4f28bae151cbc8d52e26870c25547977d196eb5ab
2fc48563c782059e0c54ca5a1c3741a991ca429434557cd070d1e87be4f7bfd7
0206b6610a84596f5fcdda5879d0fa56bb6ab45d0c88b27b25fa9d4301327db8
```

Child `PATH` retains the separate B-010 environment digest. On every platform,
effective search inputs are represented only by their frozen SHA-256 fields,
ordered attempt evidence, and resolution outcome; directory contents,
configured command, candidate paths, and raw search text are never serialized.
The supported native-binary probe child receives the exact sanitized child
`PATH` from the launch context. A shebang launcher fails before child
environment construction can select an interpreter. Other environment keys
follow only the closed policy below. This implements B-004, B-005, and the PATH
portion of B-010.

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

After owner readiness and active-deadline start, each potentially blocking
filesystem operation runs in a dedicated Linux observation subprocess. This
includes cwd open/metadata, candidate `fstatat`/`openat`, final-target boundary
classification, retained-handle read/hash, exec-stop re-hash/image
classification, and both path/hash checkpoints. No such operation runs through `spawn_blocking`, a
Tokio worker, or the owner thread. The ready owner is the sole helper spawner:
inside one synchronous, non-cancellable critical section it creates the helper,
opens its pidfd, records reap ownership, and only then exposes the client lease,
response channel, or transferred descriptor. No await or fallible ownership
handoff exists between creation and registration. The helper uses only preallocated
fixed-size frames, bounded buffers, raw syscalls, and an allocation-free,
non-panicking SHA-256 state; retained descriptors cross the private Unix socket
with `SCM_RIGHTS`. Protocol truncation, surplus fields, descriptor-count
mismatch, or a payload above its fixed bound fails typed.

Every observation reply is bounded by its closed stage budget.
`CapabilityCheck`, `WorkingDirectory`, `Candidate`, `TargetAuthorization`,
`SourceHash`, `PreSpawnCheckpoint`, `ExecStopCheckpoint`,
`PostReapCheckpoint`, and `GroupMembership` use the one active deadline;
`CleanupMembership` uses the separate cleanup deadline. If the applicable
deadline expires, the owner closes IPC and signals/reaps the exact helper pidfd
within the remaining cleanup path. The producer returns closed
`RuntimeFingerprintProduceError::ObservationDeadlineExceeded { stage }` when
cleanup completes, or
`ObservationCleanupIncomplete { stage, operation }` after the owner retains an
unreaped helper. Both return no envelope, so a missing required cwd identity or
candidate outcome is never fabricated. The closed stages are
`CapabilityCheck`, `WorkingDirectory`, `Candidate`, `TargetAuthorization`,
`SourceHash`, `PreSpawnCheckpoint`, `ExecStopCheckpoint`,
`PostReapCheckpoint`, `GroupMembership`, and `CleanupMembership`; closed
operations are `Termination`, `Reap`, and `ProtocolClose`. A reply with a
truncated or oversized frame, surplus fields, descriptor-count mismatch, or an
unexpected helper exit returns
`ObservationProtocolInvalid { stage, reason }` with closed reason
`TruncatedFrame`, `OversizedFrame`, `SurplusFields`,
`DescriptorCountMismatch`, or `HelperExited`. It is not mislabeled as a
deadline. Caller cancellation follows the owner cleanup path and returns no
envelope without fabricating an observation error. These errors contain no PID,
path, or OS diagnostic.

For the initial target, the subprocess opens the selected target once and
operates on that file handle. It first uses path metadata only to skip a target
already known to be non-regular, then calls `libc::open` with
`O_RDONLY | O_CLOEXEC | O_NONBLOCK`; it never uses a potentially blocking
ordinary `File::open` on an unclassified path. Handle `fstat` is authoritative
after open and rejects a FIFO, socket, directory, device, or any race-swapped
non-regular target before reading. Symlinks are classified by the opened final
target. The retained regular handle remains nonblocking, which does not alter
regular-file reads, and is incrementally hashed in fixed-size chunks. Unix also
derives execute permission from handle mode bits. Windows extension/search
eligibility belongs to resolution and successful OS loading belongs to spawn;
the producer does not claim that an extension or parsed PE header proves
loadability. On a platform with supervised spawn, a bad-image or access failure
is `spawn_failed`; Windows v0.1 instead returns no-envelope producer error
`ContainmentUnavailable(UnsupportedPlatform)` before spawn and makes no
loadability claim.

Failure to create the retained inspection handle, except exact
`ENOENT`/`ENOTDIR`, is `identity/open_failed`. It occurs before metadata or content facts and is
mutually exclusive with `metadata_unavailable` and `read_failed`, both of which
require an opened handle. It may retain configured-command/search/attempt
digests but never a raw path, ACL, username, localized `io::Error`, handle
metadata, strong identity, executable digest, child, version, or cleanup claim.
For a bare Unix candidate this stops the search fail closed; absolute and
qualified candidates have no fallback. Exact `ENOENT`/`ENOTDIR` instead maps
to `Absent` under the command-form rules above.

The helper checks the fixed, non-caller-adjustable
`RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES = 67_108_864` limit against initial
metadata and again while reading, stops at byte 67,108,865, does not preallocate
the maximum, and returns only bounded typed facts. No multi-megabyte file read
occurs in the Harness process and no `std::fs::read` whole-file allocation is
allowed.

After final target authorization, the owner creates the target and remains the
sole parent-side ptrace controller and wait/reap owner. The target helper's
audited pre-exec closure is the one exception: it first
`PTRACE_TRACEME`s and stops itself before exec. The parent verifies that stop,
sets `PTRACE_O_TRACEEXEC`, and resumes only into retained-handle
`execveat(AT_EMPTY_PATH)`. Exact `EACCES`/`ETXTBSY`/`ENOEXEC` retain their
closed attempt semantics; exact `ENOENT` is terminal
`interpreter_authorization_unavailable` because the retained handle exists and
the closed-on-exec script/interpreter contract could not be satisfied. A
successful exec must deliver exactly one `PTRACE_EVENT_EXEC` before the new
image's first instruction. While it remains stopped and kernel executable
write denial is active, one registered observation helper re-hashes the
retained handle and opens/stats `/proc/<pid>/exe` to match the original strong
identity; no `/proc` image observation runs on the owner or async runtime.
Mismatch kills/reaps the stopped child and emits `identity_changed`; only an
exact match may resume. A missing or surplus exec event, abnormal trace state,
or inability to prove this ordering returns the closed no-envelope producer
error `ExecutionVerificationUnavailable`; the owner kills/reaps the stopped
child without resume, and no pathname fallback exists.

The retained strong identity is device/inode from handle metadata on Unix and
volume serial plus 128-bit `FILE_ID_INFO` from the opened handle on Windows.
Path, mtime, extension, or a weaker optional metadata field is not a fallback.
If the opened handle cannot provide the specified strong identity, the producer
records `identity/metadata_unavailable` before version attribution. Before the
pre-spawn checkpoint, platform handle APIs must also return the final target
path used by the B-007 boundary classifier. Failure to prove that target lies
outside every validated repository/worktree root records
`target_authorization_unavailable` or `probe_not_authorized` and stops without
spawn. `path_unusable` remains exclusive to resolution. Immediately before
spawn and again after the child is reaped, one blocking checkpoint re-reads and
re-hashes the retained executable handle and reopens the private candidate
reference to compare strong identity. On Unix both checkpoint reopens use the
same `O_RDONLY | O_CLOEXEC | O_NONBLOCK` flags and authoritative handle
classification as the initial open. Absolute references use `open`;
working-directory-relative references use `openat` against the same retained
directory descriptor used for initial observation and child `fchdir`.
Replacing the configured cwd pathname therefore cannot redirect resolution or
either checkpoint; a race-swapped FIFO or other special file in the retained
directory cannot block and becomes `identity_changed`.
Initial, pre-spawn, exec-stop, and post-reap retained-handle size/digest
observations must match, and both later path identities must equal the retained
handle. A symlink is identified
by the opened target, not by mixing link metadata with target bytes.

If any size, digest, or strong-identity comparison fails, the envelope records
`identity/identity_changed`, emits no version fact, and does not associate the
version output with the inspected executable digest. A pre-spawn mismatch
prevents the probe; a post-reap mismatch discards the candidate version. The
successful source correlation remains named `checkpoint_consistent_path`;
mutation and restoration entirely between path checkpoints remains a
pathname-history gap. The separate `exec_stop_consistent_handle` fact requires
the verified pre-first-instruction exec stop, stopped-image strong identity,
and matching retained-handle digest while kernel write denial is active. It
proves no changed target or interpreter instruction ran before validation and
that the resumed executable bytes equal the recorded digest. This implements
B-006 and its non-goal without promoting path history to attestation.

## Supervised Version Probe

`probe.rs` owns a fingerprint-specific `RuntimeFingerprintProbeSupervisor`
instead of treating the existing `ManagedChild` drop path as completion
evidence. A static platform matrix is checked after isolation/sandbox
validation but before owner reservation: Windows, macOS, and other Unix return
no-envelope producer error `ContainmentUnavailable(UnsupportedPlatform)`.
Linux with the required syscall surface reserves
`RuntimeFingerprintProbeSupervisor::reserve`
creates a runtime-independent owner thread and waits for its readiness
handshake under
`RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE = Duration::from_millis(1_000)`,
measured monotonically from immediately before thread creation. Bootstrap does
no blocking work before sending readiness and observing its
cancellation-aware control channel. Thread creation, closed handshake, deadline
expiry, or caller cancellation closes that channel and starts the separate
`RUNTIME_FINGERPRINT_OWNER_STOP_JOIN_DEADLINE =
Duration::from_millis(1_000)` from the stop request. The owner must join within
that second bound. Start failure, readiness timeout, and stop/join timeout use
the closed producer-error reasons `OwnerStartFailed`, `OwnerReadyTimeout`, and
`OwnerStopJoinTimeout`; failure returns no envelope, no helper or child exists,
and there is no synchronous fallback.
The owner is the lifecycle owner from the outset, not a thread constructed
during cancellation.

Immediately after owner readiness and before the observation subprocess that
opens the cwd, one private monotonic
`RUNTIME_FINGERPRINT_PROBE_DEADLINE = Duration::from_millis(5_000)` starts.
Under that deadline, the owner runs one bounded system-only capability helper:
`pidfd_open` plus signal zero, `PTRACE_TRACEME` plus parent
`PTRACE_O_TRACEEXEC` installation, an invalid-fd `execveat` probe that
distinguishes `EBADF` from `ENOSYS`, and strong `/proc` process/group/image
enumeration. The helper is atomically pidfd-owned like every observation helper
and is reaped before cwd access. Unsupported capability returns no-envelope
`ContainmentUnavailable` with one closed reason; timeout uses
`ObservationDeadlineExceeded(CapabilityCheck)`.
It covers every observation subprocess and handoff, target authorization,
traced exec-stop verification, anchor setup, every target group join,
working-directory entry and exec handshake, the optional 150 ms `ETXTBSY`
delay and second attempt, concurrent output reads, root exit, and the post-reap
observation subprocess. Expiry while an observation helper owns the current
stage returns the typed producer error above and no envelope. Expiry during
trace setup or after a target attempt begins but before the verified
`exec_started` resume returns `ExecutionVerificationUnavailable`, kills/reaps
the stopped child without resume, and emits no envelope. Only expiry after that
verified resume records `version_probe/timeout`.

The ready owner creates a minimal process-group anchor that becomes and remains
the group leader. Target helpers join the anchor's group before target handle
exec; null stdin and piped stdout/stderr are unchanged. The owner retains the
anchor control/reap handle and exact pidfds for the anchor, root, every
observation helper, and every discovered non-anchor member. Every `/proc`
membership enumeration and revalidation runs in an owner-created,
atomically pidfd-registered observation helper, never on the owner or async
runtime. `RUNTIME_FINGERPRINT_MEMBERSHIP_BATCH = 64` is fixed. Starting from
the beginning of `/proc` on every pass, the helper opens and revalidates at
most 64 exact non-anchor pidfds, transfers exactly the declared descriptor
count in one bounded `SCM_RIGHTS` response, and sets `more = true` as soon as a
65th matching member is observed. It never drops a member because a batch is
full. The owner takes ownership of the transferred pidfds, signals that batch
when cleanup is required, and launches a fresh from-the-beginning pass under
the same deadline. Repeated rescans, rather than a reusable PID cursor, avoid
PID-reuse omissions. Completion is legal only when a full pass reports zero
non-anchor members and `more = false`; continuous churn ends in the applicable
typed deadline error, never a false empty claim.

Active-path enumeration uses `GroupMembership`; cleanup-path enumeration uses
`CleanupMembership`. A stalled membership helper returns
`ObservationDeadlineExceeded` when it is reaped or
`ObservationCleanupIncomplete` when ownership remains. Malformed protocol or
unexpected helper exit returns `ObservationProtocolInvalid`; cancellation
returns no envelope through the owner cleanup path. Every case returns no
envelope—even when cleanup began from an otherwise envelope-producing probe
failure. The owner signals only transferred non-anchor pidfds. No code signals
a negative PGID, so the anchor cannot be collateral damage from group-wide
`SIGTERM` or `SIGKILL`. Normal success first proves through the bounded helper
that the anchor is the only member, then requests anchor exit through its
private control channel and reaps it. Only after the group is proven empty may
a failed control/reap path signal the anchor through its own pidfd. After
releasing the anchor, no code may query, signal, or make an ownership decision
using that numeric PGID. This is deliberately named
process-group supervision, not descendant containment: a Unix child may call
`setsid` or change process groups, so the producer can prove only root status
and observations about processes that remain in the anchored original group.
It never emits a whole-descendant-tree-empty claim. A non-escapable
Linux sandbox is outside this packet, and the closed
source-plus-opened-target policy prevents repository/worktree code from
reaching this probe.

Every terminal path reaps the root and proves that only the live anchor remains
before it can publish a result. A zero exit and valid output is only a
provisional success until that check passes and the anchor is then exited and
reaped. If another anchored-group member remains, the producer records
`version_probe/lingering_process_group`,
discards the provisional version, and starts cleanup. Existing timeout,
overflow, output-read, nonzero/signal, and parse failures also enter cleanup
whenever their root or original group remains live.

Under a separate private nonzero five-second monotonic
`RUNTIME_FINGERPRINT_CLEANUP_DEADLINE`, envelope-producing probe cleanup signals
the exact root and helper-transferred non-anchor member pidfds, drains or closes
both pipes within the remaining budget, reaps the root where possible, and
verifies through a `CleanupMembership` observation helper that only the
still-live anchor remains. The owner reserves no unbounded final wait: on
cleanup-deadline expiry it closes helper IPC, signals the exact helper pidfd,
and returns the applicable observation producer error while retaining any
unreaped ownership. Observation-helper cleanup instead returns one of the
producer errors above and never creates a partial envelope.
If all operations complete, the envelope carries only the triggering
`version_probe` failure. If an operation fails or the deadline expires, the
envelope also carries the applicable closed `lifecycle_cleanup` failure:

| Cleanup kind | Trigger |
| --- | --- |
| `termination_failed` | signalling an exact root, non-anchor member, or post-empty-group anchor pidfd failed |
| `output_drain_failed` | bounded drain did not complete before read handles were closed |
| `reap_failed` | root or anchor reap failed or was not verified |
| `group_verification_failed` | the owner could not verify that only its live anchor remained |

Original trigger failures sort before cleanup failures. Any cleanup failure
closes parent read handles, omits version and any terminated/reaped or
whole-tree claim, and leaves child/group ownership with the already-running
fingerprint-specific runtime-independent owner before returning evidence. The
owner emits an `error`, keeps ownership, and continues exact-pidfd signalling,
reaping, and
anchored-group verification; later success does not rewrite an already emitted
fingerprint. It never signals by PGID. A probe child in kernel uninterruptible
I/O may outlive the cleanup deadline; an observation helper has the separate
typed no-envelope error contract above. In either case the independent owner
continues to hold the pidfd and reap obligation while the producer future and
every in-process worker have returned, and no result claims that process
terminated. This prevents
both an escaped pipe holder and a normal root that leaves a same-group child
from blocking the API or yielding false success while remaining honest about
the limited process-group evidence.

Caller cancellation closes the client lease and activates cleanup in the same
pre-existing owner, but emits no envelope. The owner survives immediate
shutdown of the Tokio runtime that hosted the cancelled future. There is no
on-demand cleanup-thread creation and no synchronous unbounded drop fallback.
Root-only `kill_on_drop` and the existing detached Tokio `ManagedChild` reaper
are not completion evidence.

The current non-Linux `ManagedChild` has no equivalent pre-observation
kill-isolated helper plus exact-pidfd supervision. Windows, macOS, and other
Unix v0.1 therefore return no-envelope producer error
`ContainmentUnavailable(UnsupportedPlatform)` before cwd observation or spawn.
The name means the required supervision primitive is unavailable; it does not
imply that Linux process groups are non-escapable.
Assigning a Job Object after `Command::spawn` is insufficient. Atomic Windows
supervision would require suspended low-level launch, no-breakaway
kill-on-close Job Object assignment, then resume, which is outside this packet.

Before reserving the owner, allocating output buffers, or spawning any helper,
the producer validates `max_output_bytes` in the closed inclusive range
`1..=RUNTIME_FINGERPRINT_MAX_OUTPUT_BYTES`, where the v0.1 constant is exactly
65,536. Zero and 65,537 fail typed. The collector reads both pipes concurrently
in fixed-size chunks while one counter enforces the validated inclusive
`max_output_bytes` across both buffers. It reads
at most the remaining capacity plus one sentinel byte and never calls
`Command::output` or `wait_with_output`. Exactly the configured maximum is
legal; observing byte `max_output_bytes + 1` records
`output_limit_exceeded`. A bounded prefix only is retained for diagnostics
outside the canonical payload. Cleanup uses the remaining shared deadline and
may close read handles rather than waiting forever for EOF. No lifecycle or
cleanup failure can produce a version fact.

The canonical failure record contains only closed enums and compatible bounded
details: an exit code, byte limit, timeout milliseconds, closed
`RuntimeProbeAuthorizationReason`
(`ConfigurationSourceRepository` or `ResolvedTargetRepository`), closed
`RuntimeSupervisionSetupStage`, or closed cleanup operation where applicable.
It never contains `io::Error` text, localized diagnostics, raw output, raw
paths, or environment values. Define closed `RuntimeProbePhase` and
`RuntimeProbeFailureKind` enums for every row in the B-008 table. Constructors
validate legal phase/kind/detail combinations. Producers sort by phase rank and
kind rank and reject duplicates; parsers reject unknown or noncanonical input.

The closed `RuntimeFingerprintProduceError` additionally contains
`ContainmentUnavailable` with closed platform/owner/capability reasons,
`ObservationDeadlineExceeded { stage }`,
`ObservationCleanupIncomplete { stage, operation }`,
`ObservationProtocolInvalid { stage, reason }`, and
`ExecutionVerificationUnavailable`. These producer errors never construct a
partial envelope. The protocol-invalid reasons are the five closed values
defined above. `ExecutionVerificationUnavailable` covers a missing or
surplus `PTRACE_EVENT_EXEC`, an abnormal trace transition, and active-deadline
expiry before verified resume; it carries no PID, path, errno, or OS text.

The result-state matrix is fail closed:

| Earliest outcome | Allowed later facts |
| --- | --- |
| path resolution failure | no resolved identity, executable digest, or version |
| absolute/qualified non-regular or non-executable | matching identity failure and bounded inspected-handle facts only; no child, fallback, selected identity, or version |
| open failure | configured-command and resolution-attempt digests only; no handle, executable digest, child, or version |
| later identity failure | bounded opened-handle facts only; no stable executable digest/version pair |
| source/target repository probe not authorized | one `inspection_target` identity/hash may remain with the closed authorization reason; no selected/executed identity, exec attempt, fallback, child, or version |
| target authorization unavailable | inspected identity/hash may remain; no selected/executed identity, exec attempt, fallback, child, or version |
| executable/interpreter authorization unavailable before anchor | stable inspected identity/hash may remain; no loader/interpreter lookup, anchor, target, selected identity, or version exists |
| traced retained-handle execution unavailable | stable inspected identity may remain; no target instruction was allowed to run, no pathname was reopened for exec, and version is absent |
| Unix bare-name `bare_eacces_exhausted` | configured-command, search, and ordered attempt digests may remain; every inspected-candidate identity is discarded and no final executable identity, child, or version exists |
| pre-observation supervision unavailable | typed producer error and no envelope; no cwd/executable fact or child exists |
| supervision setup failure | the closed stage is anchor setup, group join, working-directory entry, or trace setup; every helper was reaped, target handle exec was not attempted, its errno cannot cause PATH fallback, and version is absent |
| pre-resume execution verification unavailable | typed producer error and no envelope; the owner kills/reaps the stopped child without resume, no target instruction runs, and no pathname fallback occurs |
| terminal pre-start `spawn_failed` | the inspected target identity may remain, but no selected/executed identity or version exists |
| post-`exec_started` lifecycle/exit/output failure | the selected/executed identity may remain; version is absent |
| root exited with a non-anchor member still in the anchored group | `lingering_process_group` is required; selected/executed identity may remain, version is absent, and cleanup must run |
| observation timeout | typed producer error and no envelope; the producer future is bounded and the independent owner continues exact helper, root, and member ownership where cleanup is incomplete |
| invalid observation protocol or helper exit | typed producer error and no envelope; the closed stage/reason is retained, no deadline is fabricated, and the owner continues exact cleanup ownership |
| probe timeout or cleanup failure | stable identity may remain; version and completed-cleanup claims are absent; the producer future is bounded and the independent owner continues exact child ownership |
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
and use a duplicate-detecting serde visitor rather than first
decoding to `serde_json::Value`, because the latter can overwrite an earlier
duplicate key. After parse and before canonicalization, the root must be an
object; root boolean, array, string, number, and null values return a typed
`RootNotObject` contract error and emit no digest. Boolean schemas remain legal
only in schema-valued child positions. Malformed JSON and
duplicate-object-key errors remain typed and occur before canonicalization or
digesting. The visitor also retains each validated raw JSON number token for
the exact B-014 encoding instead of round-tripping it through `f64`. There is
no public
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
schema traversal below the fixed safe bound without a new parser or dependency.

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

For `Draft202012`, `$defs` and `dependentSchemas` are schema maps;
`dependentRequired` enters `StringSetMap`; `prefixItems` enters
`SchemaArrayOrdered`; `items` accepts only an object or boolean schema; and
`contentSchema`, `unevaluatedItems`, and `unevaluatedProperties` are
single-schema locations. `definitions`, `dependencies`, and
`additionalItems` are extension instance data. Array-form `items` is a typed
malformed standard keyword rather than legacy tuple syntax.

For `Draft07`, `definitions` is a schema map, `dependencies` enters
`LegacyDependenciesMap`, array-form `items` enters `SchemaArrayOrdered`, and
`additionalItems` enters `Schema` only beside array-form `items`; otherwise it
is instance data. `$defs`, `dependentSchemas`, `dependentRequired`,
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

## Product-to-Test Mapping

| Product behavior | Required verification |
| --- | --- |
| B-001, B-014, B-015 | `envelope_round_trips_both_closed_subjects`; `envelope_rejects_version_subject_payload_capability_and_fingerprint_digest_mismatch`; `fingerprint_digest_is_separate_from_component_integrity`; `fingerprint_digest_framing_vectors_are_independent`; `complete_runtime_and_mcp_payload_digest_vectors_are_fixed`; `canonical_payload_string_escaping_is_frozen`; `canonical_payload_preserves_raw_json_number_tokens`; `failure_payload_changes_fingerprint_digest_without_fabricating_integrity`; `component_integrity_preserves_exact_source_bytes_or_absence` |
| B-002 | `local_executable_runtime_kind_is_closed_and_uses_fixed_args_and_output_grammars`; `container_isolation_fails_before_host_resolution`; `microvm_isolation_fails_before_host_resolution`; `sandbox_passthrough_state_is_only_supported_policy`; `restricted_sandbox_fails_before_host_observation`; `narrowed_allowed_write_paths_fail_before_host_observation`; server `runtime_fingerprint_runtime_kind_contract_is_exhaustive` |
| B-003, B-011 | `runner_observation_preserves_every_runtime_and_mcp_source_identity`; `runtime_role_sources_are_pairwise_distinct_for_one_base`; `runtime_role_source_preserves_scope_and_exact_source_integrity_or_absence`; `caller_cannot_preencode_or_override_runtime_role_source`; `runtime_role_parser_rejects_missing_malformed_noncanonical_and_wrong_role_suffixes`; `repository_owned_runtime_never_spawns_version_child`; `caller_cannot_promote_repository_source`; `configured_mcp_server_binding_uses_exact_stable_key`; `configured_mcp_server_key_accepts_1024_and_rejects_1025_before_expansion`; `arbitrary_mcp_server_component_is_not_accepted`; `distinct_mcp_server_keys_have_distinct_ids`; `mcp_tool_source_is_injective_for_multiple_tools_on_one_server`; `mcp_tool_source_preserves_scope_and_encodes_exact_utf8_identity`; `mcp_server_and_tool_suffix_mismatches_are_rejected`; `caller_cannot_supply_preencoded_mcp_tool_source` |
| B-004 | `configured_command_digest_distinguishes_missing_and_spelling_variants`; `runtime_command_form_round_trips_and_rejects_cross_form_outcomes`; `working_directory_spelling_and_unix_identity_digests_have_fixed_vectors`; `working_directory_open_failure_precedes_resolution`; `cwd_path_replacement_keeps_relative_resolution_checkpoints_and_fchdir_on_one_handle`; independent Unix raw-byte and Windows UTF-16LE fixed vectors freeze exact domains, platform tags, big-endian `u64` counts, little-endian Windows units, candidate digests, and raw-command absence; `unix_retained_handle_exec_preserves_configured_argv0`; `unix_bare_path_unset_is_path_unusable_without_default_search`; Unix `bare_path_eacces_falls_back_to_second_same_basename`; `bare_eacces_exhaustion_has_no_final_identity`; `etxtbsy_retries_same_candidate_once_after_150_ms`; `etxtbsy_checkpoint_rechecks_authorization_hash_and_path_identity`; `etxtbsy_checkpoint_authorization_change_prevents_second_exec`; `etxtbsy_checkpoint_rejects_configuration_source_reason`; `etxtbsy_checkpoint_unavailable_authorization_prevents_second_exec`; `etxtbsy_retry_group_join_failure_is_reaped_without_exec_or_fallback`; `second_etxtbsy_is_terminal`; `etxtbsy_sequence_rejects_wrong_errno_delay_count_and_outcome`; `enoexec_never_starts_a_shell`; `non_eacces_spawn_error_is_terminal_without_selected_identity`; `absolute_and_qualified_commands_never_search_fallback`; `open_enoent_and_enotdir_are_absent_with_command_form_semantics`; `open_failed_stops_bare_search_without_sensitive_diagnostics`; `absolute_and_qualified_nonregular_and_nonexecutable_require_identity_failure`; `runtime_resolution_attempts_round_trip_all_outcomes_and_exec_sequences`; `runtime_resolution_attempts_reject_illegal_state_combinations`; `authorization_unavailable_attempt_requires_matching_failure_and_no_exec`; `handle_execution_unavailable_requires_none_and_no_child`; `bare_path_accepts_exactly_64_attempts`; `bare_path_rejects_candidate_65`; `repository_inspection_target_never_execs_or_falls_back`; `non_repository_source_cannot_exec_repository_target`; `target_authorization_unavailable_prevents_exec_and_fallback`; Windows `frozen_windows_search_order_is_compiler_independent`; `current_command_differential_fails_on_frozen_resolver_drift`; `windows_resolution_context_digest_domains_and_vectors_are_fixed`; `windows_non_exe_programs_are_path_unusable` with explicit `.bat`/`.cmd` no-shell assertions; `unstable_relative_resolution_is_path_unusable` |
| B-005, B-010 | `runtime_kind_selects_closed_environment_policy`; `arbitrary_environment_key_cannot_be_declared_or_exposed`; `aws_secret_access_key_never_reaches_probe_or_evidence`; `setup_secret_exclusion_overrides_closed_policy`; `cross_runtime_environment_key_is_excluded`; `direct_and_env_shebangs_fail_before_interpreter_or_anchor`; `interpreter_authorization_unavailable_requires_none_and_no_child`; independent fixed vectors freeze PATH and `CLAUDE_CONFIG_DIR` domains, platform tags, big-endian counts, Unix raw bytes, Windows UTF-16LE, absent/empty distinction, and non-UTF-8 Unix values; `windows_path_case_variants_collide`; `windows_setup_secret_exclusion_is_case_insensitive`; `windows_non_ascii_environment_key_fails_closed`; `unix_environment_keys_remain_case_sensitive` |
| B-006 | `absolute_and_qualified_open_denial_is_open_failed`; `open_failed_is_mutually_exclusive_with_handle_failures`; `unix_fifo_socket_directory_and_device_never_block_or_reach_hashing`; `fifo_swap_at_each_checkpoint_is_nonblocking_identity_changed`; `opened_handle_drives_metadata_and_incremental_hash`; `retained_working_directory_handle_survives_path_replacement`; `executable_size_accepts_67108864_and_rejects_67108865`; `unix_execute_bits_come_from_handle`; Windows `strong_file_id_is_required_without_executable_inference`; `executable_growth_crossing_limit_is_explicit`; `all_blocking_observation_uses_kill_isolated_processes`; `owner_is_sole_target_anchor_fork_parent_ptrace_wait_reap_and_helper_spawner_except_target_traceme`; `owner_spawns_and_registers_pidfd_before_exposing_lease`; cancellation injection at every create/register boundary; `observation_protocol_rejects_bad_frames_descriptor_counts_and_helper_exit`; `every_observation_timeout_returns_typed_error_without_envelope`; `active_and_cleanup_membership_stalls_return_no_envelope`; `membership_batch_accepts_64_and_rescans_65_and_larger`; `membership_continuous_churn_expires_without_false_empty`; `candidate_open_boundary_hash_exec_stop_checkpoint_and_membership_timeouts_transfer_exact_pidfd_ownership`; `uninterruptible_helper_never_fabricates_termination`; `native_static_et_exec_and_static_pie_are_accepted`; `pt_interp_wrong_machine_bad_versions_sizes_extended_counts_bounds_and_non_elf_are_rejected_before_anchor`; `fd_cloexec_script_fails_before_interpreter_execution`; `ptrace_exec_stop_precedes_first_instruction`; `exec_stop_identity_and_hash_run_under_kernel_write_denial`; `changed_native_image_is_killed_before_first_instruction`; `missing_surplus_abnormal_and_pre_resume_timeout_never_resume_and_return_no_envelope`; `path_replacement_after_authorization_executes_verified_retained_handle_not_replacement`; Linux `missing_execveat_fails_without_path_fallback`; `path_replacement_discards_version_with_identity_changed`; `in_place_rewrite_before_spawn_discards_version`; `in_place_rewrite_during_probe_discards_version`; `checkpoint_consistent_path_does_not_attest_path_history`; `exec_stop_consistent_handle_attests_executed_digest` |
| B-007 | Linux `owner_ready_deadline_bounds_success_delay_and_cancellation`; `owner_stop_join_deadline_is_separate_and_typed`; `owner_stop_join_timeout_is_childless_containment_unavailable`; `active_deadline_starts_before_cwd_observation_and_includes_post_reap_checkpoint`; `ordinary_timeout_reaps_root_and_verifies_only_anchor_remains`; `active_and_cleanup_deadlines_are_distinct_and_fixed`; `zero_exit_with_lingering_same_group_child_is_failure_and_cleaned`; `success_reaps_root_then_anchor_without_released_pgid_use`; `pidfd_revalidation_precedes_every_non_anchor_signal`; `negative_pgid_signal_is_absent`; `anchor_is_not_signalled_until_group_is_empty`; `initial_group_setup_failure_is_reaped_without_exec`; `initial_and_retry_fchdir_failure_are_stage_tagged_reaped_without_exec_or_fallback`; `process_group_supervision_does_not_claim_non_escapable_containment`; `setsid_descendant_cannot_produce_descendant_tree_empty_evidence`; `escaped_pipe_holder_hits_cleanup_deadline_without_version`; `output_cap_rejects_0_and_65537_before_allocation_or_helper`; `output_cap_accepts_1_and_65536`; `exact_combined_output_limit_is_allowed`; `combined_output_limit_plus_one_starts_cleanup`; `cancellation_reaps_after_immediate_tokio_runtime_shutdown`; macOS/other-Unix/Windows `containment_unavailable_prevents_cwd_observation_and_spawn` |
| B-008 | `failure_vocabulary_round_trips_every_legal_pair`; `timeout_plus_cleanup_failure_round_trips_in_canonical_order`; `cleanup_failure_never_emits_version_or_reaped_claim`; `observation_deadline_cleanup_and_protocol_errors_are_closed_redacted_distinct_and_have_no_envelope`; `probe_deadline_expiry_transfers_ownership_and_returns_incomplete_evidence`; `anchor_termination_and_reap_failures_are_typed`; `failure_order_and_details_are_canonical_and_redacted`; `unknown_or_incompatible_failure_values_are_rejected` |
| B-009 | `version_parser_accepts_exact_codex_and_claude_whole_stream_grammars`; `version_parser_rejects_v_prefix_extra_text_and_dependency_versions`; `stdout_stderr_and_output_digests_are_exact`; `both_streams_are_parsed_before_selection`; `same_version_on_both_streams_is_ambiguous`; `valid_version_with_nonblank_other_stream_is_unparseable`; `blank_unparseable_ambiguous_invalid_utf8_nonzero_and_signal_are_failures` |
| B-012 | `mcp_description_preserves_absent_empty_space_tab_and_newline_distinctions`; `mcp_output_schema_absence_and_presence_are_distinct`; `mcp_annotations_preserve_absent_empty_hints_title_vendor_values_and_ordered_arrays`; `mcp_annotation_hints_do_not_infer_capabilities`; exact-limit and limit-plus-one tool-name/description/annotations fixtures |
| B-013 | `mcp_input_schema_rejects_every_non_object_root`; `mcp_output_schema_rejects_malformed_and_every_non_object_root`; `mcp_output_schema_applies_every_exact_and_limit_plus_one_bound`; `absent_schema_dialect_defaults_to_draft_2020_12`; `exact_supported_schema_dialects_round_trip`; `unknown_nonstring_and_nested_schema_dialects_fail_typed`; `schema_set_locations_reorder_canonically`; Draft 2020-12 `content_schema_traverses_nested_required_and_one_of_as_schema`; Draft-07 `content_schema_remains_ordered_instance_data`; `draft_07_dependencies_schema_and_string_set_forms_are_context_aware`; `draft_07_dependencies_reject_invalid_shapes`; `draft_2020_12_legacy_keywords_remain_instance_data`; `ordered_schema_annotation_and_extension_arrays_remain_sensitive`; `schema_keyword_shaped_annotation_keys_remain_instance_data`; `draft_2020_12_object_items_traverses_nested_schema`; `draft_2020_12_array_items_is_malformed`; `draft_07_array_items_preserves_tuple_order`; `draft_07_additional_items_traverses_schema_context`; `additional_items_without_draft_07_array_items_remains_instance_data`; `draft_2020_12_dependent_required_property_arrays_are_canonical_string_sets`; `dependent_required_rejects_non_string_set_shapes`; `boolean_items_is_canonical_nested_schema`; `raw_schema_rejects_duplicate_keys`; independent exact counting vectors pin root depth, value nodes, decoded key/value strings, direct entries, raw bytes, and canonical bytes; exact-limit and limit-plus-one fixtures for every `McpContractLimitKind`; deep/wide input does not panic; `rg` API audit proving no public `from_serializable`, `serde_json::Value`, or typed-map evidence constructor |
| B-016 | `git diff` manifest check plus `rg` call-site audit proving no production consumer |

All failure tests assert the absence of a version fact and the absence of raw
path, PATH, output, environment, and OS-diagnostic text from serialized
evidence. Ordinary explicit lifecycle tests retain exact helper/root/member
pidfds plus process-group IDs and verify root reap, only-anchor-remains, anchor
reap, absence of negative-PGID signals, and no released-PGID operation when no
cleanup failure is recorded. Fault-injection tests cover
every cleanup operation, retain
ownership before returning incomplete probe evidence or a typed no-envelope
observation error, and never claim an escaped descendant was contained.
Cancellation tests drop the hosting Tokio runtime
immediately and verify the independent owner continues cleanup. PATH tests
create multiple same-basename candidates, a directory containing spaces, and
literal shell metacharacters.
Direct and `/usr/bin/env` shebang fixtures stop before interpreter or anchor
creation. macOS, other-Unix, and Windows tests stop before cwd observation with
`containment_unavailable`. Schema expected digests are fixed independent
vectors rather than values generated by the production helper under test.

## Authorized Implementation Surface

Only these paths are authorized:

1. `crates/harness-core/src/stack/mod.rs`
2. `crates/harness-core/src/stack/fingerprint.rs`
3. `crates/harness-core/src/stack/fingerprint/model.rs`
4. `crates/harness-core/src/stack/fingerprint/schema.rs`
5. `crates/harness-core/src/stack/fingerprint/tests.rs`
6. `crates/harness-agents/Cargo.toml` (direct existing workspace `libc` only)
7. `crates/harness-agents/src/lib.rs`
8. `crates/harness-agents/src/runtime_fingerprint.rs`
9. `crates/harness-agents/src/runtime_fingerprint/environment.rs`
10. `crates/harness-agents/src/runtime_fingerprint/executable.rs`
11. `crates/harness-agents/src/runtime_fingerprint/probe.rs`
12. `crates/harness-agents/src/runtime_fingerprint/tests.rs`
13. `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs`
    (`#[cfg(test)]` exhaustive mapping contract only)

Moving the two existing inline test modules into their listed test files is
part of this scope. The agents manifest may add only `libc = { workspace =
true }`; there is no new crate/version or lockfile change. No other manifest,
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

The changed-file audit must equal the thirteen-path manifest. `Cargo.lock` must
be unchanged and `cargo tree -p harness-agents -i libc` must show the pinned
workspace dependency. A call-site audit
must show that production uses of the new APIs remain confined to their
defining modules; test uses do not count as consumers. File-length checks must
show every Rust file below 800 lines. The sandbox parity gate and Linux
retained-directory/retained-executable path require mandatory human security
review for exact `SandboxSpec` passthrough equivalence, observation-process
fixed-frame/`SCM_RIGHTS` protocol, pidfd ownership and revalidation,
allocation-free post-fork work, descriptor ownership, `fchdir` ordering and
error staging, `FD_CLOEXEC` script rejection, ptrace-stop ordering,
stopped-image identity/hash validation under kernel write denial,
non-anchor-only signalling, anchor exit ordering, argument/environment pointers, NUL validation, error
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
- Treating a Unix process group as non-escapable descendant containment is
  rejected: v0.1 records only root and original-group observations.
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
process-group and file-identity capabilities differ; unsupported guarantees
must yield the applicable typed failure rather than a warning-only fallback.
