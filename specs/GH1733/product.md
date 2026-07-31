# Product Spec

## Linked Issue

GH-1733

complexity: high

## User Problem

Harness can identify repository files and the prompt inputs it constructs, but
it cannot produce a bounded, reviewable fingerprint of the executable selected
for a configured agent runtime or of the tool contract advertised by an MCP
server. A file hash alone misses version output, launch-relevant environment
facts, MCP descriptions, and input schemas that can change agent behavior.

The fingerprint must remain honest about provenance. A runner performing an
observation does not become the owner of a repository-, user-, admin-, system-,
or runtime-owned component. Likewise, resolving or probing a configured
executable does not prove that an adapter later launched that same executable,
and exposing a producer API does not mean that Harness already collects these
facts in snapshots or through a CLI.

## Goals

- Produce versioned, deterministic ASC-001 evidence for configured local agent
  runtime executables and advertised MCP tool contracts.
- Resolve one configured executable with the same single-command and `PATH`
  semantics as its adapter, without enumerating or probing unrelated commands.
- Bound executable reads, child-process lifetime, and combined probe output,
  while recording every incomplete observation with a closed failure vocabulary.
- Preserve actual component ownership separately from runner observation and
  trust metadata.
- Canonicalize MCP schemas without erasing order that remains behavior-affecting
  inside annotations or ordered schema locations.
- Exclude setup-only secrets and persist only explicitly declared,
  behavior-affecting environment facts.

## Non-Goals

- Probing arbitrary `PATH` entries, shell expressions, package managers, MCP
  server processes, remote hosts, or executables not selected by configuration.
- Persisting credentials, setup-only secrets, raw environment values, a full
  process environment, or unbounded command output.
- Claiming that a local file hash and version probe are a cryptographic
  attestation or prove which bytes a later adapter invocation executed.
- Changing adapter launch arguments, sandboxing, capability enforcement,
  prompt construction, or current runtime selection.
- Automatically collecting fingerprints into an Agent Stack snapshot; ASC-005
  owns snapshot production and consumption.
- Adding a CLI, HTTP endpoint, persistence schema, or dashboard; ASC-026 owns
  native snapshot and diff commands.
- Fingerprinting Anthropic API or Remote Host as local executables, recording
  MCP calls, or inferring declared, granted, or observed capabilities.
- Accepting compatibility aliases, arbitrary string runtime kinds, or generic
  untyped fingerprint payloads.

## User-Visible Behavior

1. **B-001:** Every public fingerprint uses one strict outer envelope with
   schema version `agent-stack-fingerprint/v0.1`, a closed subject of
   `agent_runtime` or `mcp_tool`, one validated ASC-001 component, and the
   matching versioned typed payload. The runtime payload declares
   `runtime-executable-fingerprint/v0.1`; the MCP payload declares
   `mcp-tool-fingerprint/v0.1`. Public serialization and parsing consume this
   envelope, reject missing or unknown versions, subjects, and fields, and
   cannot pair a subject with the other payload type. The v0.1 component
   capability list is always empty because this evidence does not infer
   capabilities; constructors emit an empty list and parsers reject a nonempty
   one. No exported schema constant exists without a corresponding wire field
   and parser check.
2. **B-002:** A runtime fingerprint accepts a closed typed local-executable
   kind with exactly `codex_exec`, `codex_jsonrpc`, and `claude_code`. These
   values correspond one-to-one to the locally launched workflow
   `RuntimeKind` variants without adding a dependency from `harness-agents` to
   `harness-workflow`; any later consumer performs an exhaustive typed mapping
   at its boundary. `anthropic_api`, `remote_host`, unknown strings, aliases,
   case variants, UUIDs, display labels, and caller-supplied hex encodings
   cannot be converted into a local runtime identity. Version arguments come
   from the closed mapping for the selected kind and are not an arbitrary
   public command vector.
3. **B-003:** Every producer accepts or derives a validated ASC-001 source from
   persisted ownership information. Repository-, user-global-, admin-, system-,
   and runtime-owned runtime or MCP components retain that source scope and
   locator when observed by a runner. Only a genuinely runner-owned executable
   or tool definition uses `runner` source scope. The emitted observation class,
   trust, selection state, and freshness are `runner_observed`,
   `runner_observed`, `observed`, and `fresh`; stronger observation never
   rewrites the component ID. Invalid, ambiguous, or untyped ownership fails
   before a fingerprint is emitted.
4. **B-004:** Runtime resolution treats the configured executable as exactly
   one command name or path and never invokes a shell or parses embedded
   arguments, quoting, pipes, substitutions, or redirections. A path with a
   directory component resolves only relative to the declared working
   directory or as an absolute path. A bare name searches the exact sanitized
   `PATH` used for the probe in native order and selects according to the same
   launch contract as the adapter. Resolution checks only that configured
   basename, never executes `which`, never runs candidates while searching,
   and never falls through to a later candidate after selecting the executable
   that normal launch semantics would select.
5. **B-005:** The version child receives a minimal declared environment. It
   retains the same sanitized `PATH` used by B-004 so `#!/usr/bin/env`
   launchers can find their interpreter, and receives only other keys whose
   typed declaration explicitly permits probe exposure. Every name in
   `codex.cloud.setup_secret_env` is excluded from both the child environment
   and persisted fingerprint facts regardless of spelling; sensitivity is not
   guessed from substrings such as `TOKEN` or `SECRET`. No setup-only value can
   reach `codex --version` or an equivalent child.
6. **B-006:** Executable identity comes from one opened regular-file handle,
   not separate path-based metadata and content reads. Size, executable status,
   and SHA-256 cover the bytes read from that handle. The producer enforces the
   configured byte ceiling before and during reading, performs potentially
   blocking file hashing outside the async executor, and does not allocate the
   declared maximum eagerly. Before spawn and after child completion it checks
   that the resolved path still identifies the opened file using the strongest
   stable file identity available on the platform. A detected replacement or
   an inability to link version output to the inspected identity is explicit;
   version facts are not attributed to the digest. This is local observation,
   not a claim that path execution is race-free or cryptographically attested.
7. **B-007:** A version probe has one lifecycle covering spawn, concurrent
   stdout/stderr reads, exit, timeout, and reap. Stdout and stderr are drained
   incrementally under one hard combined byte limit. Reaching the output limit,
   reaching the timeout, dropping/cancelling the future, or encountering a read
   failure terminates and reaps the child and descendants covered by the launch
   contract; no probe process is left running. Output is never fully buffered
   and then truncated. Limit or timeout evidence contains no success-shaped
   version fact.
8. **B-008:** Probe incompleteness is represented by closed typed phase and
   kind values, not caller-defined strings. The v0.1 vocabulary is the table
   below. A failure record contains bounded, redacted structured facts only,
   such as an exit code or limit; localized OS messages, raw paths, output, and
   environment values are not part of the canonical digest. Failures serialize
   in deterministic phase/kind order, and an unsupported phase/kind is rejected.
9. **B-009:** Version success requires a stable executable identity, exit code
   zero, bounded valid UTF-8 output, and exactly one unambiguous version token.
   The v0.1 token grammar is an optional single `v` or `V`, at least two
   dot-separated numeric components, and optional ASCII SemVer-style
   prerelease/build suffix. Normalization removes only the leading `v`/`V` and
   preserves the remaining byte spelling and case. The exact bounded stdout and
   stderr byte digests and the selected stream are retained. Successful but
   blank output records `empty_output`; nonblank output with no token records
   `unparseable_version`; conflicting candidates record `ambiguous_version`.
   None may yield `failures = []` or fabricate a normalized value.
10. **B-010:** Runtime environment evidence uses an explicit typed allowlist of
    behavior-affecting keys for each supported runtime. Keys are unique and
    sorted; missing values are `unset`; allowed non-secret values are represented
    only by SHA-256; explicitly sensitive runtime values are `redacted` without
    a value digest. Probe exposure is a separate typed decision from evidence
    inclusion. Blank, NUL-containing, duplicate, undeclared, full-environment,
    and setup-secret entries are rejected or excluded according to B-005 rather
    than silently reclassified by a name heuristic. `PATH` is recorded only as
    a digest plus resolution outcome, never as raw directory content.
11. **B-011:** An MCP tool producer requires a validated stable tool source and
    exact server/tool identities. The source describes component ownership;
    runner observation describes who obtained the advertised contract. A
    repository- or user-configured MCP server therefore does not become a
    `runner:mcp_tool:*` component merely because the runner queried it. Tool
    identity is derived through ASC-001 component-ID construction, and blank,
    generated per-observation, UUID/display-alias, or ownership-free source
    identities fail closed.
12. **B-012:** MCP tool name and optional description are fingerprinted exactly
    as advertised in their UTF-8 string values. The producer does not trim,
    collapse, case-fold, Unicode-normalize, or otherwise rewrite whitespace or
    punctuation. `None`, an empty description, a single space, repeated spaces,
    tabs, and newlines remain distinct payload facts and produce distinct
    digests when their exact advertised values differ.
13. **B-013:** MCP input-schema canonicalization is context-aware. JSON object
    member order is canonicalized lexicographically. Only arrays at the closed
    order-insensitive schema-keyword locations `required`, `type`, `enum`,
    `allOf`, `anyOf`, and `oneOf` are sorted, and schema-valued children are
    traversed as schemas. Ordered schema locations such as `prefixItems`, all
    unknown/vendor-extension arrays, and instance-valued annotations including
    `default`, `const`, `examples`, and `example` preserve array order at every
    depth. A key named `enum`, `required`, or `oneOf` inside annotation data is
    ordinary instance data and does not activate schema sorting. Duplicate JSON
    object keys are rejected before canonicalization rather than overwritten.
14. **B-014:** Canonical fingerprint digests cover the exact payload schema
    version and every behavior-affecting typed fact, with lexicographic object
    keys and stable collection ordering. They exclude timestamps, run IDs,
    localized diagnostics, raw secret values, and the outer ASC-001 component
    to avoid self-reference. The resulting lowercase SHA-256 is the component
    integrity. Reordering JSON object members or a B-013 order-insensitive set
    leaves the digest unchanged; changing source-independent runtime facts,
    exact MCP description text, an ordered annotation, or a failure kind changes
    it. Aggregate ordering and stack IDs remain ASC-005 responsibilities.
15. **B-015:** Invalid producer inputs, invalid source evidence, unsupported
    runtime kinds, malformed schema JSON, and impossible envelope combinations
    return typed errors and emit no fingerprint. Expected observation failures
    such as a missing executable or unavailable version are successful evidence
    records only when they contain the applicable B-008 failure and omit every
    unsupported fact. Missing data remains absent; no empty digest, sentinel
    path, placeholder version, runner-owned alias, or warning-only fallback is
    substituted.
16. **B-016:** This issue exposes deterministic producer APIs in
    `harness-core` and `harness-agents` plus contract tests. It does not invoke
    them from `CodeAgent`, `AgentAdapter`, the workflow runtime, task runner,
    server startup, snapshot assembly, or a command. Existing execution and
    public wire behavior remain unchanged until ASC-005 or ASC-026 adds an
    explicit consumer. Documentation and tests say “can produce” rather than
    claiming that Harness already collected or used a fingerprint.

### Runtime Probe Failure Vocabulary

| Phase | Kind | Meaning |
| --- | --- | --- |
| `path_resolution` | `path_not_found` | No executable selected by the configured path/`PATH` launch contract. |
| `path_resolution` | `path_unusable` | The launch contract selected a path that cannot be represented or inspected safely. |
| `identity` | `metadata_unavailable` | Metadata could not be read from the opened handle. |
| `identity` | `not_regular_file` | The selected target is not a regular file. |
| `identity` | `not_executable` | The selected target is not executable under the platform launch contract. |
| `identity` | `executable_too_large` | The byte ceiling was exceeded before or during hashing. |
| `identity` | `read_failed` | The opened executable could not be read completely. |
| `identity` | `identity_changed` | Path identity did not remain linked to the opened handle across the probe. |
| `version_probe` | `spawn_failed` | The selected command could not be spawned. |
| `version_probe` | `timeout` | The lifecycle deadline expired and the child was terminated and reaped. |
| `version_probe` | `output_limit_exceeded` | Combined stdout/stderr crossed the hard byte limit and the child was terminated and reaped. |
| `version_probe` | `output_read_failed` | Either output pipe failed before a complete bounded result was obtained. |
| `version_probe` | `nonzero_exit` | The child exited with a nonzero code. |
| `version_probe` | `terminated_by_signal` | The child terminated without an exit code. |
| `version_probe` | `invalid_utf8` | Bounded output was not valid UTF-8. |
| `version_probe` | `empty_output` | Exit was successful but both streams were blank. |
| `version_probe` | `unparseable_version` | Nonblank output contained no v0.1 version token. |
| `version_probe` | `ambiguous_version` | Output contained conflicting candidate version tokens. |

## Acceptance Criteria

- [ ] Public positive and negative fixtures round-trip both strict B-001
      envelope subjects and reject version, subject, payload, and unknown-field
      mismatches, including a component with any nonempty capability list.
- [ ] Runtime APIs accept only the closed typed local-executable kind, use fixed
      version arguments, and cannot launder arbitrary strings, UUIDs, display
      aliases, or pre-encoded hex into component identity; an exhaustive
      mapping test covers every workflow `RuntimeKind` without reversing the
      crate dependency direction.
- [ ] Source fixtures prove repository-, user-, admin-, system-, runtime-, and
      genuinely runner-owned runtime/MCP components keep identical component
      IDs when observation strengthens to `runner_observed`.
- [ ] PATH fixtures prove bare-name first-match launch parity, qualified and
      working-directory-relative paths, an absent command, duplicate basenames,
      path entries containing spaces, shell-metacharacter non-interpretation,
      and refusal to probe any unrelated executable.
- [ ] An interpreter-launcher fixture proves that sanitized `PATH` is identical
      for resolution and child execution, while a setup-only secret named
      `NPM_ACCESS` is absent from the child and from serialized facts.
- [ ] Identity fixtures prove handle-based metadata/hash consistency, before-
      and during-read size limits, nonblocking async execution, and explicit
      `identity_changed` evidence when the selected path is replaced between
      inspection and probe.
- [ ] Lifecycle fixtures use hanging and unbounded dual-stream children to
      prove timeout, cancellation, and combined output-limit paths terminate and
      reap the child without unbounded allocation or a version fact.
- [ ] Failure fixtures cover every B-008 phase/kind, deterministic ordering,
      sanitized bounded facts, and rejection of unknown values.
- [ ] Version fixtures cover stdout, stderr, leading `v`/`V`, prerelease/build
      case preservation, nonzero and signalled exit, invalid UTF-8, successful
      blank output, unparseable output, and conflicting candidates.
- [ ] Environment fixtures prove declared set/unset/digest/redacted behavior,
      raw-value and raw-PATH absence, duplicate/invalid-key rejection, separate
      probe exposure, and unconditional exclusion of every setup-secret key.
- [ ] MCP fixtures prove stable ownership, exact tool/description sensitivity,
      and distinct absence, empty, spaces, tabs, and newline descriptions.
- [ ] Schema fixtures prove object-key and approved set reordering stability;
      ordered `prefixItems`, vendor arrays, and nested arrays under `default`,
      `const`, `examples`, and `example` remain digest-sensitive even when an
      annotation object contains a schema-keyword-shaped key.
- [ ] Duplicate JSON keys and malformed schemas fail typed before digesting.
- [ ] Focused and package suites pass with
      `cargo check -p harness-agents -p harness-core --all-targets`,
      `cargo test -p harness-core fingerprint`, and
      `cargo test -p harness-agents runtime_fingerprint`.
- [ ] A changed-file and call-site audit proves there is no snapshot, CLI,
      server, workflow-runtime, `CodeAgent`, or `AgentAdapter` consumer and no
      persistence, dependency, or existing launch-behavior change.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-002, B-009, B-011, B-012, and B-015; blank identities and output are explicit errors or failure evidence. |
| Error and failure paths | Covered by B-006 through B-009 and B-015; every incomplete observation uses the closed vocabulary. |
| Authorization / permission | Covered by B-004, B-005, B-010, and B-016; the producer gains no shell, arbitrary-command, snapshot, or runtime authority. |
| Concurrency / race / ordering | Covered by B-006, B-007, B-008, B-013, and B-014; handle identity, child lifecycle, and canonical ordering are explicit. |
| Retry / repetition / idempotency | Covered by B-008 and B-014; unchanged bounded facts produce identical records and digests. |
| Illegal state transitions | N/A. Producers are stateless; B-001 and B-015 reject impossible evidence combinations. |
| Compatibility / migration | Covered by B-001, B-002, and B-016; this is additive producer-only code with no persisted or existing public wire migration. |
| Degradation / fallback | Covered by B-008, B-009, and B-015; unavailable data becomes typed failure evidence, never a placeholder success. |
| Evidence and audit integrity | Covered by B-003 and B-006 through B-015; ownership, observer, bytes, version, schema, failures, and redaction remain distinguishable. |
| Cancellation / interruption / partial completion | Covered by B-007 and B-015; cancellation reaps children and cannot publish a success-shaped partial probe. |

## Edge Cases

- The configured command is a bare name and two `PATH` directories contain it.
- A qualified relative executable contains spaces or shell metacharacters.
- An npm-style launcher uses `#!/usr/bin/env node` and the interpreter is found
  only through the sanitized `PATH`.
- A setup-only secret has a harmless-looking name such as `NPM_ACCESS`.
- The executable is a symlink, is replaced during hashing, or is replaced and
  restored around the version probe.
- A regular file grows past the byte limit after its first metadata read.
- A probe hangs, forks, closes only one stream, floods both streams, exits by
  signal, succeeds with blank output, or emits conflicting versions.
- Version output contains invalid UTF-8 or valid Unicode surrounding one ASCII
  version token.
- An MCP description differs only by tabs, repeated spaces, or line breaks.
- A schema annotation contains `{ "enum": [1, 2] }` as ordinary default data.
- `prefixItems` or a vendor extension differs only by array order.
- An MCP tool is advertised by a runner but owned by repository configuration.
- A caller attempts to use a UUID, display label, or arbitrary string as a
  runtime kind or source locator.

## Rollout Notes

The producer APIs are additive and initially have no automatic consumer, so
they require no migration or feature flag and cannot alter current execution.
ASC-005 may later place these envelopes into canonical snapshots, and ASC-026
may expose snapshot collection through native CLI commands. Those consumers
must preserve the B-003 ownership/observation split and must not reinterpret a
probe record as proof that an adapter executed the inspected bytes.
