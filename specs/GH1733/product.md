# Product Spec

## Linked Issue

GH-1733

Runtime-specific B-004 through B-010 behavior is split into
`runtime-product.md`; implementation details are split between
`runtime-observation.md` and `runtime-supervision.md`. Read all six packet
files listed in `tasks.md`.

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
- Resolve one configured executable with the adapter-compatible safe subset of
  its single-command and `PATH` semantics, with every fail-closed divergence
  documented and without enumerating or probing unrelated commands.
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
- Claiming that v0.1 isolates an already authorized executable from read-only
  same-UID host filesystem or process state. The producer controls probe
  `envp` and persisted evidence; a complete read sandbox requires a later
  separately reviewed isolation design.
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
   cannot pair a subject with the other payload type. Each envelope also has a
   required `fingerprint_digest` over its versioned subject and canonical
   payload. This digest is separate from ASC-001 `component.integrity`, whose
   meaning remains SHA-256 of exact source bytes or absence. The v0.1 component
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
   public command vector. The same mapping owns a whole-output version grammar:
   both Codex kinds accept only `codex-cli <VERSION>`, and Claude Code accepts
   only `<VERSION> (Claude Code)`, apart from one optional final line ending.
   Runtime producer input also carries the existing closed execution isolation
   value and the adapter's effective `SandboxSpec`. v0.1 accepts only `host`
   plus the exact passthrough sandbox state
   `DangerFullAccess` with `allowed_write_paths = None`; its payload records the
   closed fact `sandbox_policy = danger_full_access_unrestricted`.
   `container` and `microvm` retain their typed unsupported-isolation errors.
   `ReadOnly`, `ReadOnlyWithNetwork`, `WorkspaceWrite`, or any narrowed
   allowed-write-path state fails separately with
   `sandbox_parity_unavailable`. Both gates precede PATH resolution,
   working-directory or executable access, and process creation. v0.1 never
   runs an unrestricted version child for an adapter launch that would be
   restricted.
3. **B-003:** Every producer accepts or derives a validated ASC-001 source from
   persisted ownership information. Repository-, user-global-, admin-, system-,
   and runtime-owned runtime or MCP components retain their source scope and
   persisted ownership identity when observed by a runner; the closed runtime
   and MCP bindings below may derive a typed locator from that base identity.
   Only a genuinely runner-owned executable or tool definition uses `runner`
   source scope. The emitted observation class, trust, selection state, and
   freshness are `runner_observed`,
   `runner_observed`, `observed`, and `fresh`; stronger observation never
   rewrites the component ID. Invalid, ambiguous, or untyped ownership fails
   before a fingerprint is emitted. This bounded producer accepts a validated
   ASC-001 base source locator of at most 4,096 UTF-8 bytes and a complete
   derived runtime/server/tool locator of at most 8,259 UTF-8 bytes. These are
   fingerprint-local limits, not changes to global ASC-001 validity. All
   derivation uses checked length arithmetic and validates before suffix
   allocation or copying. `from_exact_source_bytes` accepts at most 2,097,152
   source bytes and checks byte 2,097,153 before copying or SHA-256 work; this
   fingerprint-local constructor limit does not change global ASC-001 validity.
   Strict envelope input is at most 2,097,152 raw bytes and is rejected before
   JSON allocation when larger. Exact limits are accepted; limit-plus-one fails
   with the matching closed limit kind. The closed
   `RuntimeFingerprintLimitKind` values are `exact_source_bytes`,
   `base_source_locator_bytes`, `derived_source_locator_bytes`, and
   `envelope_bytes`. Raw-envelope size has first precedence before JSON
   allocation; strict parsing then checks complete derived-locator size before
   suffix decoding and recovered-base size. Exact-source construction checks
   source byte length before base size, checked suffix expansion, complete
   derived size, copying, or hashing; construction without source bytes begins
   at the base-size check.
   Runtime component identity is a typed role
   binding derived internally from the base source and exact B-002 runtime-kind
   spelling:
   `<base_locator>/harness_agent_runtime_role_v0_1/u<byte_length>_<lowercase_utf8_hex>`.
   This injective mapping preserves scope and gives `codex_exec`,
   `codex_jsonrpc`, and `claude_code` distinct component IDs even when two
   roles share one persisted base binding. Callers cannot supply or pre-encode
   the role locator. Envelope parsing strips the final two locator segments,
   validates the remaining base source under the same scope, decodes the exact
   length-prefixed suffix, requires one canonical runtime-kind spelling equal
   to the payload kind, re-derives the locator, and rejects missing, malformed,
   noncanonical, or wrong-role suffixes. If the base binding has exact
   canonical source bytes, each derived role retains their identical ASC-001
   integrity; otherwise integrity is absent. Role text and fingerprint payload
   never enter component integrity.

Runtime-specific behavior B-004 through B-010 and its closed failure and
attempt vocabularies are normative in `runtime-product.md`.

11. **B-011:** An MCP tool producer requires a typed configured-server binding
    made from a validated ownership source and the exact stable key of a
    persisted MCP configuration entry; it does not accept an arbitrary
    prebuilt `mcp_server` component, display label, UUID, or session identity.
    The stable key is at most 1,024 UTF-8 bytes; the exact limit is accepted
    and limit-plus-one fails typed before hexadecimal expansion or locator
    allocation. After UTF-8 validation it is rejected as blank exactly when it
    is empty or every byte is HT (`0x09`), LF (`0x0a`), CR (`0x0d`), or SP
    (`0x20`). There is no trimming or Unicode whitespace predicate; VT, FF,
    NBSP, and every other valid UTF-8 scalar are nonblank and remain exact
    identity bytes. The advertised tool-name nonblank check uses the same
    predicate.
    The binding derives the server locator as
    `<base_locator>/harness_mcp_server_config_v0_1/u<byte_length>_<lowercase_utf8_hex>`
    from that exact nonblank UTF-8 key. The tool producer also requires the
    exact advertised tool name; callers cannot supply a separate or pre-encoded
    server or tool source. A typed v0.1 mapping preserves the server source
    scope and derives the tool locator as
    `<server_locator>/harness_mcp_tool_v0_1/u<byte_length>_<lowercase_utf8_hex>`.
    The length-prefixed exact UTF-8 encoding is reversible, injective, and
    case-sensitive, so multiple tools from one server cannot collide. Runner
    observation describes who obtained the contract and never changes ownership
    scope. The derived structured tool binding has no canonical raw source
    bytes, so ASC-001 component integrity is absent. Strict parsing validates
    both derived suffixes and requires the payload server ID and tool name to
    match them. Blank stable keys or names, wrong server kind, malformed or
    noncanonical derived locators, payload mismatches, and caller-supplied
    encodings fail closed. This contract binds identity to the exact stable
    configuration key; it does not claim historical proof that a caller kept
    the same key across observations.
12. **B-012:** MCP tool name, optional description, optional `annotations`,
    required `inputSchema`, and optional `outputSchema` are fingerprinted as
    advertised. Absence of `outputSchema` remains distinct from any present
    schema. Both schemas use
    the same duplicate-aware, object-root, bounded canonicalization contract in
    B-013. `annotations` accepts only absent or a raw JSON object through the
    same duplicate-aware parser; absent and `{}` remain distinct. Its object
    keys canonicalize, while all values use `InstanceData`, so exact booleans
    such as `readOnlyHint`, `destructiveHint`, `idempotentHint`, and
    `openWorldHint`, exact title/vendor values, and array order affect the
    digest without populating the empty ASC capability list. Tool text is
    retained in its exact UTF-8 string value; the producer
    does not trim,
    collapse, case-fold, Unicode-normalize, or otherwise rewrite whitespace or
    punctuation. `None`, an empty description, a single space, repeated spaces,
    tabs, and newlines remain distinct payload facts and produce distinct
    digests when their exact advertised values differ. v0.1 permits a nonblank
    tool name of at most 1,024 UTF-8 bytes and a description of at most 65,536
    UTF-8 bytes. Annotation limits are fixed independently at 65,536 raw bytes,
    49,152 canonical bytes, depth 32, 4,096 JSON nodes, 32,768 cumulative
    decoded-string UTF-8 bytes, and 1,024 entries in one object or array. Exact
    limits are allowed and limit-plus-one fails typed before fingerprint
    construction.
13. **B-013:** Every present MCP input or output schema must have a JSON object
    at its root; a root boolean, array, string, number, or null fails typed
    before canonicalization or digest construction. Boolean schemas remain
    legal only at schema-valued child positions. The root dialect is frozen
    before keyword traversal: absent `$schema` means Draft 2020-12; the only
    accepted explicit values are exactly
    `https://json-schema.org/draft/2020-12/schema` and
    `http://json-schema.org/draft-07/schema#`. A non-string, unknown value, or
    `$schema` at any nested schema position fails typed
    `unsupported_schema_dialect`; nested dialect changes are outside v0.1.
    The exact `$schema` member remains canonical payload data.

    JSON object member order is canonicalized lexicographically. In both
    dialects, arrays at `required`, `type`, `enum`, `allOf`, `anyOf`, and
    `oneOf` are order-insensitive under their closed contexts. Draft 2020-12
    additionally treats `$defs`, `dependentSchemas`, `dependentRequired`,
    `prefixItems`, `contentSchema`, and `unevaluatedItems` /
    `unevaluatedProperties` according to that dialect; `dependentRequired`
    property arrays are string sets, `prefixItems` remains an ordered schema
    array, and `items` accepts only an object or boolean schema. In both
    dialects, `not`, `if`, `then`, `else`, `contains`, `propertyNames`, and
    `additionalProperties` are single object-or-boolean schema positions even
    when a neighboring activating keyword is absent. Other value shapes fail
    typed `malformed_single_schema_keyword` with a closed dialect/keyword
    detail, and nested `$schema` at any of these positions is rejected. In
    Draft-07, `definitions`, legacy `dependencies`, and array-form `items`
    receive their legacy schema or string-set semantics; `additionalItems`
    always remains a schema-valued keyword, even without array-form `items`.
    Under Draft-07 `contentSchema`, `dependentRequired`, `dependentSchemas`,
    `prefixItems`, and other later-draft keywords are extension instance data;
    under Draft 2020-12 `dependencies`, `definitions`, `additionalItems`, and
    array-form `items` are extension instance data or a typed malformed
    standard keyword as applicable. Thus Draft-07 `contentSchema` arrays retain
    order and cannot be collapsed by Draft 2020-12 keyword rules.

    Unknown/vendor-extension values and instance-valued annotations including
    `default`, `const`, `examples`, and `example` preserve array order at every
    depth. A key named `enum`, `required`, or `oneOf` inside instance data is
    ordinary data and does not activate schema sorting. Public evidence
    construction accepts only raw JSON text or bytes through a duplicate-aware
    parser. It exposes no `serde_json::Value`, generic serializable, or
    typed-map constructor that could claim duplicate-free original input after
    an ordinary decoder overwrote a key. `harness-core` explicitly enables the
    existing `serde_json/raw_value` feature. The private visitor borrows each
    member/element as `RawValue`, recursively visits its validated source slice,
    and retains number-leaf `RawValue::get()` text so `1`, `1.0`, `1e0`, and
    long valid tokens remain distinct. No handwritten lexer, new package, or
    lockfile change is permitted.

    Resource limits are fixed by v0.1 and are not caller-adjustable. For each
    present schema: raw bytes are at most 1,048,576; canonical bytes at most
    786,432; nesting depth at most 64; total JSON nodes at most 65,536;
    cumulative decoded-string UTF-8 bytes at most 524,288; and any single
    object or array at most 4,096 entries. Exact limits are valid.

    Depth is the number of JSON value nodes on a root-to-value path, with the
    root at depth 1. Every JSON value, including the root, object member value,
    and array element, counts as one node; object keys are not nodes. Decoded
    string bytes count each object key and string value after JSON unescaping,
    once per occurrence. Container entries count direct object members or array
    elements. On entering a value the visitor checks depth, then increments and
    checks nodes; before accepting a member/element it increments and checks
    that container's entries; after decoding a key or string value it adds and
    checks its UTF-8 bytes. Raw size is checked first; canonical-byte budget is
    checked only after duplicate-aware parse and structural budgets succeed.
    A limit-plus-one condition returns its exact closed typed limit error,
    emits no digest or envelope, and cannot panic or overflow the stack.
14. **B-014:** The envelope `fingerprint_digest` covers the exact subject,
    payload schema version, and every behavior-affecting typed payload fact.
    Hash input is frozen as
    `domain || u64be(subject_len) || subject_utf8 ||
    u64be(inner_version_len) || inner_version_utf8 ||
    u64be(payload_len) || canonical_payload_utf8`, where `domain` is exactly
    `b"harness_agent_stack_fingerprint_digest_v0_1\0"` and every length counts
    bytes. Canonical payload JSON has no insignificant whitespace; object keys
    sort by decoded UTF-8 bytes; arrays retain typed order unless B-013 declares
    the location a set; `null`, booleans, and typed integers use
    lowercase/minimal JSON spelling; strings escape only quote, backslash, and
    U+0000..U+001F, using `\b`, `\t`, `\n`, `\f`, `\r` where applicable and
    lowercase `\u00xx` otherwise, while other Unicode scalars are emitted as
    UTF-8 and slash is never escaped. Raw MCP JSON number tokens are validated
    and preserved byte-for-byte, so `1`, `1.0`, and `1e0` remain distinct
    contract facts rather than passing through a floating-point formatter.
    The normative payload bytes `{"a":1,"z":"\n"}` (hex
    `7b2261223a312c227a223a225c6e227d`) produce digest
    `3f45cc1b14c0099eaf056f9475aa210b4f84d45b2a4940ecff35079b3b1611fe`
    for subject/version `agent_runtime` /
    `runtime-executable-fingerprint/v0.1`, and
    `e00eca6b5f5a3fe3494cf590e68ec59f70e40ee54b7f7f42e48756d296fa85d9`
    for `mcp_tool` / `mcp-tool-fingerprint/v0.1`. These framing vectors are
    independent of production serialization; tests also pin one complete valid
    payload vector for each subject. The digest excludes timestamps, run IDs,
    localized diagnostics, raw secret values, and the outer ASC-001 component
    to avoid self-reference. ASC-001 component integrity
    independently preserves exact-source-byte evidence when one exists and is
    absent otherwise; payload or failure changes never fabricate or overwrite
    it. Reordering JSON object members or a B-013 order-insensitive set leaves
    the fingerprint digest unchanged; changing runtime facts, exact MCP
    description text, an ordered annotation, or a failure kind changes it.
    Aggregate ordering and stack IDs remain ASC-005 responsibilities.
15. **B-015:** Invalid producer inputs, invalid source evidence, unsupported
    runtime kinds, non-host isolation, malformed or over-limit schema JSON, and
    impossible envelope combinations return typed errors and emit no
    fingerprint. Expected observation failures such as a missing executable,
    unauthorized repository probe, unavailable version, or incomplete cleanup
    are successful evidence records only when they contain the applicable
    B-008 failure and omit every unsupported fact. Missing data remains absent;
    no empty digest, sentinel path, placeholder version, runner-owned alias,
    fingerprint-as-integrity substitution, or warning-only fallback is used.
    Because every Windows v0.1 producer returns no-envelope containment failure,
    a v0.1 envelope constructor or parser rejects all Windows command forms and
    any present Windows resolution context; pure Windows digest helpers do not
    make those states reachable evidence.
16. **B-016:** This issue exposes deterministic producer APIs in
    `harness-core` and `harness-agents` plus contract tests. It does not invoke
    them from `CodeAgent`, `AgentAdapter`, the workflow runtime, task runner,
    server startup, snapshot assembly, or a command. Existing execution and
    public wire behavior remain unchanged until ASC-005 or ASC-026 adds an
    explicit consumer. Documentation and tests say “can produce” rather than
    claiming that Harness already collected or used a fingerprint.

## Acceptance Criteria

- [ ] Public positive and negative fixtures round-trip both strict B-001
      envelope subjects and reject version, subject, payload, and unknown-field
      mismatches, including a component with any nonempty capability list;
      fixtures prove fingerprint digest is separate from ASC-001 integrity and
      failure changes never fabricate or overwrite exact-source-byte integrity.
- [ ] Runtime APIs accept only the closed typed local-executable kind, use fixed
      version arguments, and cannot launder arbitrary strings, UUIDs, display
      aliases, or pre-encoded hex into component identity; an exhaustive
      mapping test covers every workflow `RuntimeKind` without reversing the
      crate dependency direction. Container and microVM inputs fail before any
      host resolution, file access, or process creation. Restricted sandbox
      modes and any allowed-write-path narrowing fail
      `sandbox_parity_unavailable` at the same boundary; only the exact
      passthrough sandbox state is serialized.
- [ ] Source fixtures prove repository-, user-, admin-, system-, runtime-, and
      genuinely runner-owned runtime/MCP components keep identical component
      IDs when observation strengthens to `runner_observed`; repository-owned
      runtime fixtures retain identity/hash evidence but emit
      `probe_not_authorized` without executing a marker program, and callers
      cannot promote that source to an executable trust class. Non-repository
      source fixtures resolving inside a repository/worktree boundary do the
      same, while missing or ambiguous target-boundary evidence emits
      `target_authorization_unavailable`. Registered
      `Observation(...)` helpers required for retained identity, hash, and
      authorization evidence may run, but neither case creates an
      `InitialTarget` or `RetryTarget`, runs a target/loader/interpreter
      instruction, or falls through to another PATH candidate. Runtime-role
      fixtures prove the three derived IDs are pairwise distinct for one base
      source, preserve scope and identical exact-source integrity or absence,
      and cannot be caller pre-encoded. Strict parser fixtures reject missing,
      malformed, noncanonical, and payload-wrong role suffixes. All
      runtime/server/tool bindings accept 4,096-byte base locators and reject
      4,097 before copying, use checked derivation, and reject complete
      locators above 8,259 bytes. A maximum base plus maximum stable key and
      tool name reaches exactly 8,259 bytes. Strict envelope parsing accepts
      2,097,152 raw bytes and rejects byte 2,097,153 before JSON allocation;
      exact source construction accepts 2,097,152 bytes and rejects byte
      2,097,153 before copy or SHA-256;
      fixtures pin the four closed limit reasons and their precedence.
- [ ] PATH fixtures prove Unix child-working-directory semantics and Windows
      frozen search order/`.exe` behavior, including Windows refusal to use
      `PATHEXT`, accept any explicitly named non-`.exe` extension, execute
      `.bat`/`.cmd` through a shell, or guess a qualified-relative base; both
      platforms cover an absent command, duplicate basenames, spaces, literal
      shell metacharacters, one absolute selected probe path, and no unrelated
      executable. Unix bare-name fixtures prove exact `EACCES` fallback to a
      later same-basename candidate, one exact 150-millisecond `ETXTBSY` retry
      with a fresh authorization/identity checkpoint, terminal second
      `ETXTBSY`, terminal `ENOEXEC` without `/bin/sh`, terminal other errors,
      and no search fallback for absolute or qualified commands. Injected open
      denial is `open_failed`, mutually exclusive with handle-based failures,
      and never leaks OS diagnostics. Exact `ENOENT`/`ENOTDIR` during open
      remains `absent`; bare search continues, while an absolute/qualified sole
      candidate becomes `path_not_found`. Existing non-regular or
      mode-ineligible absolute/qualified paths retain their matching identity
      failure instead of masquerading as not found. Separate late-race
      retained-handle `ENOENT` and `ENOTDIR`
      fixtures prove the fingerprint stops with
      `interpreter_authorization_unavailable`, does not execute a later PATH
      candidate, and records the documented security divergence even when the
      adapter would continue searching.
- [ ] Unix bare-name PATH-unset fixtures return `path_unusable` without
      observing a default search path, while absolute and qualified commands
      remain representable; Windows conformance compares the frozen resolver
      against the adapter's current `Command` behavior and fails on drift
      instead of adopting it.
- [ ] Launch-input fixtures accept 65,536 and reject 65,537 exact OS units for
      every closed value field before hashing/splitting/owner admission.
      Checked joins accept the mathematically maximal 196,610-unit Unix lexical
      candidate; no public input can reach 196,611 after those field gates.
      Environment keys/setup-secret names and their collection
      counts accept 1,024 and reject 1,025 before canonicalization or value
      access. They cover Unix non-UTF8 bytes, Windows surrogate-pair UTF-16
      counting, one huge PATH entry, whole PATH, and every closed limit reason.
      A 65-entry PATH selecting or terminally rejecting an earlier entry never
      reaches the candidate ceiling, while 64 nonterminal attempts followed by
      entry 65 yields `candidate_limit_exceeded`. Excluded over-limit
      PATH/Claude values and
      undeclared over-limit values are never read, hashed, or reported as value
      limits; selected counterparts fail closed. Limit failure produces no
      digest, owner, fd, cwd observation, helper, child, truncation, or fallback.
- [ ] Unix attempt fixtures round-trip every closed outcome, reject illegal
      source/outcome/failure/terminal combinations, preserve duplicate PATH
      entries and order, cover exactly 64 candidates and candidate 65, and
      prove `bare_eacces_exhausted` retains no final executable identity.
- [ ] Repository bare-name fixtures with an injected first-candidate `EACCES`
      condition and a second marker prove only the first static inspection
      target is hashed, no exec attempt or fallback occurs, and no
      selected/executed claim is emitted.
- [ ] Configured-command digest fixtures freeze Unix raw-byte and Windows
      UTF-16LE vectors, distinguish absent commands and spelling/path variants
      under otherwise identical search facts, and prove raw command text is
      absent from serialized evidence. Independent hard-coded vectors also
      freeze the exact platform tags, `u64` big-endian counts, candidate digest
      domain, and Windows little-endian `u16` units.
- [ ] Windows resolution-context fixtures freeze the four exact fields, domains,
      absent/present states, UTF-16LE framing, and independent `C:\X` vectors
      for current-executable directory, system directory, Windows directory,
      and parent PATH.
- [ ] Linux launch fixtures prove bare, qualified, and absolute probes use
      `FD_CLOEXEC` retained-handle `execveat(AT_EMPTY_PATH)`, stop at
      `PTRACE_EVENT_EXEC` before the first target instruction, and resume only
      after stopped-image identity plus retained-handle hash match while
      preserving exact configured command bytes as `argv[0]`. A call-site
      audit proves the owner is the sole target/helper fork, parent-side ptrace
      controller, wait/reap, and observation-helper-spawn authority; the
      target pre-exec closure's audited `PTRACE_TRACEME` is the sole exception.
      After verified initial exec, syscall-entry fixtures prove `fork`, `vfork`,
      `clone`, `clone3`, `execve`, `execveat`, executable `mmap`/`mprotect`,
      executable `shmat`, x86_64 `uselib`, `ptrace`, `process_vm_writev`, `userfaultfd`,
      `io_uring_setup`, `pidfd_getfd`, `recvmsg`/`recvmmsg`, `prctl`, `openat2`,
      non-query `personality`, and write-capable/truncating open-family requests are
      stopped before kernel execution, yield the exact closed
      transitive-denial class, execute no second-image/child/mapping/mutation
      marker, and suppress version. Exact `/proc/self/mem`, thread-self,
      numeric-pid, and mount-alias write opens are denied without pathname
      parsing; read-only opens remain legal. Received `SCM_RIGHTS`, remote-fd
      duplication, external-ptrace authorization, and both mmap and mprotect
      `READ_IMPLIES_EXEC` attempts are denied. The exact
      `personality(0xffff_ffff)` query is resumed, returns through the ordinary
      entry/exit trace transition, and does not produce denial evidence; every
      other argument is denied. Every `openat2` entry is denied without reading
      attacker-owned `open_how` memory, so an external same-UID writer cannot
      change approved flags after validation. Native `bpf`, `init_module`, and
      `finit_module` entries are denied as kernel-code loading regardless of
      command or current capabilities. Missing/untagged/unreadable syscall stops
      return no envelope and cleanup the target.
      A static aux-vector fixture proves each direct-exec attempt records
      `linux_fd_cloexec_execveat_empty_path_fd_10`, observes exact
      `AT_EXECFN = "/dev/fd/10"`, cannot reopen fd 10 after exec, and does not
      misstate pathname-launch parity. A preceding reaped first-candidate
      `EACCES` followed by a second-candidate pre-target classifier rejection
      preserves both attempt records and applies the no-child invariant only
      to the second attempt. It also proves the first registered target is
      reaped and removed from the exact pidfd registry before search continues;
      injected termination and reap failures append independent
      lifecycle-cleanup evidence and retain ownership.
      The owner opens the
      declared child directory once with
      `O_PATH | O_DIRECTORY | O_CLOEXEC` and no `O_NOFOLLOW`; fixed
      working-directory spelling/identity
      digests enter the payload, both initial and retry helpers `fchdir` that
      retained handle, and injected `fchdir` failure is stage-tagged, reaped,
      and cannot exec or PATH-fallback. Replacing the configured cwd pathname
      after that open proves qualified and relative-PATH observation, initial
      open, both checkpoints, and initial/retry helpers all remain anchored to
      the same retained directory identity. A search/execute-only directory
      that denies ordinary read access remains usable through the retained
      `O_PATH` handle.
- [ ] Linux executable-format fixtures prove static `ET_EXEC` and static-PIE
      `ET_DYN` success for the exact native machine tuple. Direct and
      `#!/usr/bin/env` shebangs, `PT_INTERP`, same-class/endianness
      wrong-machine ELF, wrong header version/size, W+X `PT_LOAD`, missing,
      duplicate, or executable `PT_GNU_STACK`, extended or out-of-bounds program headers, and
      non-ELF/binfmt inputs yield
      `interpreter_authorization_unavailable` before target, loader, or
      interpreter creation. Sanitized `PATH` cannot select an interpreter, and
      a setup-only secret named `NPM_ACCESS` never enters probe `envp` or
      fingerprint evidence. A supported static
      native-binary fixture separately proves sanitized `PATH` is the only
      child environment key and that any later PATH/cwd helper launch or
      executable mapping is denied before execution.
- [ ] Identity fixtures prove handle-based metadata/hash consistency, before-
      and during-read fixed 67,108,864-byte size limit plus limit-plus-one,
      kill-isolated observation subprocess execution, Unix
      nonblocking-open rejection of FIFOs and other special files, Unix
      mode-bit checks, Windows strong file-ID checks without a fabricated
      executable claim, pre-spawn/post-reap retained-handle rehashing, and
      explicit `identity_changed` evidence for path replacement or in-place
      rewrite. Initial in-repository/outside hard-link aliases fail
      `multiple_hard_links` before spawn; an unlinked retained target is the
      distinct `unlinked_target` reason, and unavailable link metadata is the
      distinct `link_count_unprovable` reason. New or removed links at
      pre-spawn, retry, exec-stop, and post-reap checkpoints produce the
      stage-appropriate authorization or identity failure. Retry fixtures
      require one reaped `ETXTBSY` helper and prove no second helper or exec,
      while a stable single-link target remains eligible. Exact 0/1/2 counts
      and unavailable exec-stop/post-reap observations exercise the closed
      outcomes above. These tests do not claim bind-mount alias exclusion.
      Delayed cwd open, candidate open, boundary lookup, read/hash, and
      both later checkpoint fixtures prove the active deadline starts before
      observation, every observation timeout returns a typed producer error
      with no envelope, the producer future returns boundedly, no
      `spawn_blocking` job survives inside the process, and an unreaped helper
      stays owned with explicit incomplete cleanup rather than a termination
      claim. Cancellation at every capability, observation, initial-target,
      and retry-target create/register boundary proves the owner atomically
      registers the exact pidfd and reap obligation before `GO` or any lease.
- [ ] Authorization-race fixtures replace and rewrite the source after the
      final checkpoint and prove the exec-stop hash/identity gate kills a
      changed native image before its first instruction, while an introduced
      shebang fails before interpreter execution. After mandatory ptrace
      containment passes, exact `ENOSYS`, `EPERM`, and fixed-call `EINVAL` from
      `execveat(10, "", ..., AT_EMPTY_PATH)` each emit an inspected
      `handle_execution_unavailable` envelope, reap every created helper, and
      never fall back to a pathname. Fixtures cover each errno on the initial
      call and after exact first-call `ETXTBSY`, requiring `single` and the
      retry sequence respectively; exact `EACCES` remains the separate
      bare-name fallback case. Missing ptrace exec-stop or tagged syscall guarding instead
      returns no-envelope
      `containment_unavailable/post_exec_guard_unavailable` before cwd
      observation. Other
      platforms return typed no-envelope `containment_unavailable` first under
      the frozen matrix. Separate missing-event, surplus-event, abnormal-trace,
      and pre-resume-deadline fixtures return
      `execution_verification_unavailable` with no envelope and prove the owner
      kills/reaps without ever resuming the stopped target.
- [ ] Lifecycle fixtures use hanging and unbounded dual-stream targets to
      reject output caps 0 and 65,537 before allocation and prove 1, 65,536,
      exact-limit, and limit-plus-one behavior. Owner readiness and stop/join
      use their separate one-second deadlines; the active five-second deadline
      begins before cwd observation and covers every helper, initial/retry
      target, exec-stop verification, output capture, exact target reap, and
      post-reap checkpoint. Cleanup has its own five-second deadline.
      Initial and retry setup injection covers exactly
      `working_directory_enter` and `trace_setup`; the registered target is
      reaped without handle exec or PATH fallback. Termination, reap, and drain
      failures produce canonical lifecycle evidence where envelope-capable and
      otherwise the closed no-envelope observation error. Success proves the
      exact registered target is reaped, streams are complete, the post-reap
      checkpoint passed, and the pidfd registry is empty. Cleanup signals and
      reaps only registered target/helper pidfds; no PGID, negative PID,
      `/proc` membership, or post-reap PID operation exists.
      Guard fixtures prove no process-creation syscall executes, so the claim is
      registry-empty rather than descendant-tree-empty. Cancellation transfers
      ownership to the pre-existing owner, survives immediate Tokio shutdown,
      and emits no evidence. An unrelated same-session process continuously
      changing process groups never enters evidence or the registry, is never
      signalled, and does not change success. macOS, other Unix, and Windows fail before cwd
      observation or process creation.

- [ ] Strict envelope fixtures reject every `WindowsBare`, `WindowsAbsolute`,
      and `WindowsQualified` runtime command form and any present Windows
      resolution context in v0.1. Pure Windows resolver/digest helpers remain
      testable but cannot construct or parse a v0.1 fingerprint envelope.
- [ ] Post-exec guard fixtures prove every target-initiated `kill`, `tkill`,
      `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`, and
      `pidfd_send_signal` entry is classified as `process_signalling`, denied
      before kernel execution, and cleaned up without a version. External
      fatal `SIGSEGV` and `SIGTERM` delivery is reinjected from
      `AwaitEntry` and yields `terminated_by_signal`; caught or ignored
      signals continue normally, and a genuine delivered `SIGTRAP` is
      distinguished from ptrace traps. Illegal-state delivery, group stops,
      malformed siginfo, and cleanup-originated signals never become target
      signalling or semantic exit evidence; capture failure still has priority.
      Direct `SIGKILL` death is accepted from `AwaitEntry` or `AwaitExit` as
      `terminated_by_signal`, but from `AwaitInitialExecExit` it is
      `ExecutionVerificationUnavailable`; each state has a fixture.
- [ ] On x86_64, both dangerous and otherwise harmless syscall numbers carrying
      `__X32_SYSCALL_BIT` fail no-envelope execution verification before native
      classification and are never normalized by clearing the bit. Native
      x86_64 and aarch64 syscall fixtures retain their existing classifications.
- [ ] Owner-capacity fixtures retain exactly eight permanently stalled owners
      and prove the ninth fails before thread/fd/cwd/process creation; API
      return, cancellation, cleanup-incomplete, and stop/join timeout do not
      release a permit, while actual owner exit with an empty registry does.
      Each owner has exactly two pidfd slots and 28 non-pidfd slots, with
      post-`DescriptorsReady` retained ceilings of 40 descriptors per
      fingerprint, 16 pidfds globally, and 320 descriptors globally. Before
      readiness, at most one bootstrap child per owner may transiently inherit
      the process-wide fd table in addition to an admitted target; it performs
      no workload and has no numeric ledger-derived ceiling. The active deadline
      bounds waiting for readiness; on expiry the owner starts exact direct-child
      rollback under the cleanup deadline and retains the obligation and permit
      until reap. After
      readiness one child retains at most 12 allowlisted references, while a
      post-exec target retains three stdio references and a concurrent exec-stop
      observer at most five; no other phase has two live child roles. There is
      no anchor, membership helper, member batch, PGID, or membership-transfer
      slot.
      Two-owner and eight-owner interleavings inspect
      `/proc/<pid>/fd` at `DESCRIPTORS_READY` and find exactly the role
      allowlist. A child stalled before readiness may retain a foreign marker
      until exact rollback/reap; one stalled after readiness retains exactly
      its allowlist and no other owner's gate, output, or control descriptors.
      Owner-side
      self-pidfd fixtures cover preflight success and every
      `pidfd_open`/signal-zero failure before the capability child and cwd,
      then require successful `waitid(P_PIDFD, WEXITED | WNOWAIT)` observation
      plus consuming `waitid(P_PIDFD, WEXITED)` of the zero-exit capability
      child. `WNOWAIT` errors/mismatches and consuming-call errors while the
      child remains unreaped exercise exact-PID bootstrap rollback. A successful
      consuming wait with malformed identity, code, or status proves no later
      positive-PID operation occurs. Completed rollback returns pidfd-unavailable;
      incomplete fallback returns cleanup-incomplete and retains the obligation
      and permit. Neither case starts cwd or any later child.
      Start-gate fault injection covers the `CapabilityCheck` and every other
      observation stage plus `InitialTarget` and `RetryTarget` roles. No role
      performs workload before `GO`;
      each exact pidfd and reap obligation is atomically registered first, or
      the gated direct child is rolled back by its still-unreaped positive PID.
      Logical-slot exhaustion and post-reservation `EMFILE` retain their
      distinct capacity and registration-stage errors.

- [ ] Failure fixtures cover every B-008 phase/kind, deterministic ordering,
      sanitized bounded facts, rejection of unknown values, and a fixed vector
      that orders simultaneous cleanup kinds as termination, reap, then
      output-drain regardless of observation order.
- [ ] Version fixtures cover exact current Codex and Claude product lines,
      stdout/stderr selection, prerelease/build case preservation, CRLF/LF,
      rejected `v`/`V`, leading/trailing text, extra dependency versions,
      nonzero and signalled exit, invalid UTF-8, successful blank output,
      one valid stream plus nonblank invalid output, same/different versions on
      both matching streams, unparseable output, and conflicting matching
      streams. Blank fixtures cover empty and every mixture of HT/LF/CR/SP as
      allowed, VT/FF/NUL/NBSP as nonblank, lone CR as legal only in the
      unselected blank stream. Precedence fixtures prove capture overflow/read
      failure wins first; after complete capture, signal or nonzero exit is the
      sole semantic failure; only zero exit can yield invalid UTF-8, blank,
      unparseable, or ambiguous output. Implementation audits forbid generic
      whitespace predicates.
- [ ] Environment fixtures prove the runtime-kind policy table's
      set/unset/digest/redacted behavior, arbitrary and cross-runtime keys
      cannot be declared or exposed, raw-value and raw-PATH absence, setup
      secrets override the closed policy, Unix comparison remains
      case-sensitive, and Windows canonical comparison rejects `Path`/`PATH`
      collisions and non-ASCII keys. Independent hard-coded PATH and
      `CLAUDE_CONFIG_DIR` vectors freeze both exact domains, platform tags,
      `u64` counts, Unix raw bytes, Windows UTF-16LE units, absent versus empty,
      and non-UTF-8 Unix input; every Windows vector is a pure helper vector and
      never becomes v0.1 envelope evidence.
- [ ] MCP fixtures prove exact stable configuration-key binding, distinct
      server IDs for distinct keys, injective typed source derivation for
      multiple exact UTF-8 tool names on one server, rejection of arbitrary
      prebuilt server components and caller-supplied encoded sources, strict
      server/tool suffix parsing, absent tool component integrity, exact
      tool/description sensitivity, and distinct absence, empty, spaces, tabs,
      and newlines; exact 1,024-byte and rejected 1,025-byte stable keys are
      checked before locator expansion. Stable keys and tool names reject empty
      or all-HT/LF/CR/SP bytes, preserve mixed nonblank input exactly, and treat
      VT, FF, and NBSP as nonblank without trimming. Absent, empty, standard-hint, title,
      vendor-value, and ordered-array annotation fixtures remain distinct under
      the bounded raw-object contract without inferring ASC capabilities.
- [ ] Schema fixtures cover required `inputSchema` and absent, present,
      malformed, non-object, exact-limit, and limit-plus-one `outputSchema`;
      reject every non-object root before canonicalization and
      prove object-key and approved set reordering stability. Absent and exact
      Draft 2020-12 roots agree; exact Draft-07 roots select legacy semantics;
      unknown/non-string root or any nested `$schema` fails typed. In Draft
      2020-12, ordered `prefixItems`, `contentSchema` nested schemas,
      `dependentRequired`, and modern schema maps are context-aware while
      legacy keys remain extension data. In Draft-07, array `items`,
      `additionalItems`, `definitions`, and `dependencies` use legacy
      semantics while a `contentSchema` value with ordered arrays remains
      instance data; `additionalItems` traverses schema context even without
      tuple `items`. In both dialects every `not`, `if`, `then`, `else`,
      `contains`, `propertyNames`, and `additionalProperties` object/boolean
      child traverses schema context, rejects another value shape with its
      closed keyword detail, rejects nested `$schema`, and canonicalizes nested
      sets. The same key names under vendor extensions, `default`, `const`,
      `examples`, and `example` remain ordered instance data and digest-
      sensitive in both dialects.
- [ ] Duplicate JSON keys and malformed raw schemas fail typed before digesting,
      and public API/call-site audit proves no generic serializable or
      `serde_json::Value` evidence constructor exists. The core manifest
      explicitly enables `serde_json/raw_value`; borrowed recursive
      `RawValue::get()` tests distinguish `1`, `1.0`, `1e0`, long valid tokens,
      malformed number syntax, and the canonical long-number boundary without
      a handwritten lexer or lockfile change.
- [ ] Tool-name, description, annotation, raw-schema, depth, node,
      decoded-string, per-container, and canonical-byte limit fixtures cover
      exact limits and limit-plus-one. Independent fixtures pin root depth one,
      value-only node counts, decoded object-key plus string-value bytes, and
      direct container entries; deep/wide hostile input fails typed without a
      digest, envelope, panic, unbounded allocation, or stack overflow.
- [ ] Independent digest fixtures freeze the exact B-014 domain, all three
      `u64` frames, canonical string escaping, raw JSON number-token
      preservation, both normative framing vectors, and one complete valid
      payload vector per subject.
- [ ] Focused and package suites pass with
      `cargo check -p harness-agents -p harness-core --all-targets`,
      `cargo test -p harness-core fingerprint`, and
      `cargo test -p harness-agents runtime_fingerprint`.
- [ ] A changed-file and call-site audit proves there is no snapshot, CLI,
      server, workflow-runtime, `CodeAgent`, or `AgentAdapter` consumer and no
      persistence or existing launch-behavior change. Private core validation
      and test submodules may split only to keep the mandatory state-machine and
      product-to-test matrix below the 800-line hard ceiling. The only dependency
      manifest change is a direct `harness-agents` dependency on the existing
      workspace `libc`.
      `Cargo.lock` may change only by adding that direct dependency edge to the
      existing `harness-agents` package entry; no package, version, source, or
      checksum change is allowed.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-002, B-009, B-011, B-012, and B-015; blank identities and output are explicit errors or failure evidence. |
| Error and failure paths | Covered by B-006 through B-009 and B-015; every incomplete observation uses the closed vocabulary. |
| Authorization / permission | Covered by B-004, B-005, B-007, B-010, and B-016; repository-owned configuration and resolved repository/worktree targets are never executed, ambiguous target ownership fails closed, and the producer gains no shell, caller policy, snapshot, or runtime authority. |
| Concurrency / race / ordering | Covered by B-006, B-007, B-008, B-013, and B-014; handle identity, child lifecycle, and canonical ordering are explicit. |
| Retry / repetition / idempotency | Covered by B-008 and B-014; unchanged bounded facts produce identical records and digests. |
| Illegal state transitions | N/A. Producers are stateless; B-001 and B-015 reject impossible evidence combinations. |
| Compatibility / migration | Covered by B-001, B-002, and B-016; this is additive producer-only code with no persisted or existing public wire migration. |
| Degradation / fallback | Covered by B-008, B-009, and B-015; unavailable data becomes typed failure evidence, never a placeholder success. |
| Evidence and audit integrity | Covered by B-003 and B-006 through B-015; ownership, observer, bytes, version, schema, failures, and redaction remain distinguishable. |
| Cancellation / interruption / partial completion | Covered by B-006, B-007, and B-015; cancellation or deadline expiry signals only registered target/helper pidfds, transfers cleanup to a runtime-independent owner, and cannot publish partial success evidence. |

## Edge Cases

- The configured command is a bare name and two `PATH` directories contain it.
- A qualified relative executable contains spaces or shell metacharacters.
- On Unix, an npm-style launcher uses `#!/usr/bin/env node`; v0.1 detects the
  shebang and fails before searching sanitized child `PATH` or starting an
  interpreter.
- A setup-only secret has a harmless-looking name such as `NPM_ACCESS`.
- Windows observation input contains `Path` and `PATH`, a case-variant setup
  secret, or a non-ASCII environment key.
- A repository-owned marker executable would write or access the network if
  invoked; fingerprinting must stop after identity/hash evidence.
- A user-global binding resolves to a repository executable, or final-target
  containment cannot be proven; observation helpers may derive retained
  evidence, but neither case may create a target child or execute target,
  loader, or interpreter instructions.
- The executable is a symlink, is replaced, is overwritten in place, or is
  changed and restored between observation checkpoints.
- A candidate path names a FIFO, socket, directory, or device and must return
  without a blocking read open.
- Cwd or executable observation stalls in kernel I/O; the producer deadline
  returns a typed error and no envelope while the independent owner retains the
  exact helper pidfd and no in-process blocking worker survives.
- A regular file grows past the byte limit after its first metadata read.
- A probe hangs, attempts to fork, closes only one stream, floods both streams, exits by
  signal, succeeds with blank output, or emits conflicting versions.
- A target attempts `fork`, `clone`, or `clone3`; the guarded syscall is
  denied before execution, so success never relies on descendant enumeration.
- A zero-exit target is reaped but an output stream or post-reap identity
  checkpoint remains incomplete; the exact-pidfd success barrier rejects it.
- Version output contains invalid UTF-8 or valid Unicode surrounding an
  otherwise valid product line; both are explicit failures.
- An MCP description differs only by tabs, repeated spaces, or line breaks.
- A schema annotation contains `{ "enum": [1, 2] }` as ordinary default data.
- `prefixItems` or a vendor extension differs only by array order.
- Draft 2020-12 and Draft-07 schemas use the same extension key with different
  keyword semantics; nested `$schema` is rejected rather than switching
  dialect mid-document.
- Tool text or schema input is exactly at, then one unit beyond, every fixed
  v0.1 resource limit.
- An MCP tool is advertised by a runner but owned by repository configuration.
- One MCP server advertises two exact tool names that differ only by case,
  Unicode bytes, slash placement, or a prefix of the other name.
- A caller attempts to use a UUID, display label, or arbitrary string as a
  runtime kind or source locator.

## Rollout Notes

The producer APIs are additive and initially have no automatic consumer, so
they require no migration or feature flag and cannot alter current execution.
ASC-005 may later place these envelopes into canonical snapshots, and ASC-026
may expose snapshot collection through native CLI commands. Those consumers
must preserve the B-003 ownership/observation split and must not reinterpret a
probe record as proof that an adapter executed the inspected bytes.
