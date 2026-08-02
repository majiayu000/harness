# Tech Spec

## Linked Issue

GH-1734

## Product Spec

See `specs/GH1734/product.md`.

<!-- specrail-planned-changes
{"issue":1734,"complete":true,"paths":["crates/harness-agents/src/runtime_fingerprint.rs","crates/harness-agents/src/runtime_fingerprint/tests.rs","crates/harness-core/src/stack/fingerprint/model.rs","crates/harness-core/src/stack/fingerprint/tests.rs","crates/harness-core/src/stack/inventory/mod.rs","crates/harness-core/src/stack/mod.rs","crates/harness-core/src/stack/snapshot.rs","crates/harness-core/src/stack/snapshot/canonical.rs","crates/harness-core/src/stack/snapshot/model.rs","crates/harness-core/src/stack/snapshot/tests.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012","B-013","B-014","B-015","B-016"]}
-->

## Current System and Root Cause

`harness_core::stack` implements ASC-001 components and ASC-002 repository
inventory. A component contains canonical identity, observation, selection,
integrity, capabilities, trust, and freshness. Repository inventory adds
`AgentStackEntryClass` outside the component, including the Unix executable
tri-state and directory-presence evidence.

The workflow runtime implements ASC-003 context provenance in
`prompt_packet/context_provenance.rs`. Its entry keeps semantic `order`,
selection `reason`, and safe memory metadata outside the component. The type is
currently private to the server module, so snapshot integration must use a
typed adapter rather than serializing it to `serde_json::Value` and reparsing
it.

GH-1733 specifies a strict core fingerprint envelope. Its
`fingerprint_digest` covers the typed runtime or MCP payload but intentionally
does not replace the outer component or ASC-001 exact-source integrity.
Snapshot identity must therefore cover both the validated component semantics
and the verified fingerprint digest.

The root cause is an absent aggregate contract, not an unstable sort in one
existing function. A plain component vector would omit producer facts; hashing
serialized snapshots would include volatile metadata and depend on incidental
wire order.

## Dependency and Implementation Gate

Specification work is allowed because GH-1734 is `ready_to_spec`.
Implementation requires all of the following:

1. the GH-1732 remediation product and tech specs are approved, GH-1732 is
   `ready_to_implement`, and that remediation is merged;
2. GH-1733 product and tech specs are approved, GH-1733 is
   `ready_to_implement`, and its implementation is merged;
3. the strict fingerprint envelope and digest contract are available in
   `harness-core`; and
4. GH-1734 itself has approved product/tech specs and
   `ready_to_implement`.

GH-1732 owns its remediation and component-integrity compatibility fixes.
After that implementation merges, GH-1734 replaces the remaining private
context-reason strings with a closed enum while preserving their exact wire
spellings and adds the snapshot conversion. The two issues must not write
`context_provenance.rs` concurrently. The unapproved PR #1859 is not a
dependency substitute. If its implementation is repaired later, GH-1734
consumes only the approved envelope API.

## Module and Ownership Design

Add a facade plus three private modules:

- `stack/snapshot.rs`: public re-exports and typed constructors;
- `stack/snapshot/model.rs`: closed value types, coverage, metadata, evidence,
  entries, accessors, and typed errors;
- `stack/snapshot/canonical.rs`: explicit framed stable projection and bounded
  SHA-256 calculation;
- `stack/snapshot/tests.rs`: fixed vectors, invariance, sensitivity, conflicts,
  limits, and producer adapters.

`stack/mod.rs` is already near the 800-line ceiling. It receives only the
module declaration and re-exports. Every new production and test file remains
below 800 lines, with a 200-400-line target.

The server context-provenance module owns conversion of its private typed entry
to a core snapshot contribution. It exposes no generic JSON conversion and
adds no automatic snapshot call site. Repository conversion stays in core.
The GH-1733 envelope is already a core type, so fingerprint conversion creates
no reverse dependency on `harness-agents`.

## Data Model

The conceptual public model is:

```text
AgentStackSnapshot
  schema_version: "agent-stack-snapshot/v0.1"
  stable_id: AgentStackStableId
  coverage: AgentStackSnapshotCoverage  // derived, not caller-supplied
  entries: Vec<AgentStackSnapshotEntry>
  observation: AgentStackSnapshotObservation

AgentStackSnapshotCoverage
  repository_inventory: Observed | NotObserved
  runtime_context: Observed | NotObserved
  runtime_fingerprint: Observed | NotObserved
  mcp_fingerprint: Observed | NotObserved

AgentStackSnapshotObservation
  created_at: DateTime<Utc>
  run_id: Option<AgentStackSnapshotRunId>

AgentStackSnapshotEntry
  component_id: AgentStackComponentId
  evidence: Vec<AgentStackSnapshotEvidence>

AgentStackSnapshotEvidence
  RepositoryInventory {
    component: AgentStackComponent,
    entry_class: AgentStackEntryClass
  }
  RuntimeContext {
    component: AgentStackComponent,
    semantic_order: u64,
    selection_reason: AgentStackContextSelectionReason,
    memory_metadata: Option<AgentStackContextMemoryMetadata>
  }
  Fingerprint {
    envelope: AgentStackFingerprintEnvelope
  }

AgentStackContextMemoryMetadata
  record_id: AgentStackContextRecordId
  evidence_ref: Option<AgentStackContextEvidenceRef>
  estimated_tokens: u64

AgentStackDomainObservation<T>  // opaque public struct
  state: DomainObservationState<T>  // private

DomainObservationState<T>  // private enum
  NotObserved
  Observed(Vec<T>)
  Failed(AgentStackProducerFailure)

AgentStackRuntimeFingerprintObservation(  // opaque public newtype
  AgentStackDomainObservation<AgentStackFingerprintEnvelope>
)

AgentStackMcpFingerprintObservation(  // opaque public newtype
  AgentStackDomainObservation<AgentStackFingerprintEnvelope>
)

AgentStackSnapshotInputs
  repository_inventory: AgentStackDomainObservation<AgentStackInventoryEntry>
  runtime_context: AgentStackDomainObservation<AgentStackRuntimeContextEvidence>
  runtime_fingerprint: AgentStackRuntimeFingerprintObservation
  mcp_fingerprint: AgentStackMcpFingerprintObservation
```

Each wrapper owns an `observed(Vec<AgentStackFingerprintEnvelope>)` typed
constructor that validates the required runtime or MCP subject and permits an
empty vector. This is the supported path for the all-observed empty vector.
The corresponding producer adapter maps its actual producer `Result` into the
same wrapper. There is no conversion between the wrappers, so the snapshot
constructor cannot exchange the two slots even though both contain the same
closed envelope type. A failed observation still carries no domain; the
wrapper and destination slot determine that domain.

Harness-core owns one closed `AgentStackContextSelectionReason` enum whose
variants map one-to-one to the existing runtime constants:

- `workflow_runtime_profile_selected`
- `workflow_base_selected`
- `workflow_repository_selected`
- `workflow_document_effective`
- `workflow_defaults_selected`
- `repo_memory_selected`

GH-1734 changes the private server reason field to that core enum only after
the GH-1732 remediation is merged; it does not define a second server enum.
The core enum's `Serialize` output is byte-for-byte equal to the existing
strings, so the prompt-packet contract does not change. The server adapter
passes the complete typed entry to core. Core itself frames the component,
semantic order, reason, and optional memory metadata; it does not accept a
caller-supplied context digest. Raw memory payload is not duplicated. Every
context component must have present integrity. For selected memory, that
integrity already covers the exact redacted packet representation defined by
GH-1732, including agent-visible `created_at` and `use_count`.

`AgentStackStableId` is a distinct newtype rather than an alias for
`Sha256Digest`. `AgentStackSnapshotRunId` is a bounded nonblank UTF-8 newtype;
it carries no parsing alias and is never used as stable identity.

All fields are private with read-only accessors. Construction accepts the four
domain observations plus observation metadata; it never accepts coverage and
evidence as separate values. Coverage is derived as `observed` or
`not_observed`. Any `Failed` value returns `observation_failed` before grouping
or hashing. Official adapters take the producer `Result` and create exactly
one of these states without a warning-only fallback. There is no public
component-only constructor because it would silently omit the facts carried by
inventory, context, or fingerprints. The opaque observation type exposes no
public state variant. Its only no-result constructor is named
`not_observed_without_attempt`; every result-taking constructor maps `Err` to
the private `Failed` state.

## Grouping and Consistency

The constructor validates each underlying component or envelope, then groups
evidence by exact component-ID UTF-8 bytes.

Within one group:

1. every component ID must be identical;
2. kind, source scope, and source locator must match;
3. if two observations both have integrity, their digests must match;
4. collect the complete canonical-byte multiset for each closed evidence kind;
5. if one such multiset contains distinct values, return
   `inconsistent_observation`;
6. otherwise, if its count exceeds one, return
   `duplicate_component_evidence`; and
7. each evidence kind requires its derived coverage domain to be `observed`.

Differing observation class, selection state, capabilities, trust, or freshness
remain distinct evidence rather than being overwritten. They are semantic
change-control facts and enter the stable projection. Absent integrity may
coexist with matching present integrity; the absence remains visible in its
own evidence record. Two different present integrity values indicate a
non-atomic or inconsistent observation and fail.

Compatibility is:

| Existing evidence for component | New evidence | Result |
| --- | --- | --- |
| none | any validated kind | accept |
| repository | repository, byte-identical | `duplicate_component_evidence` |
| repository | repository, different class/mode/component semantics | `inconsistent_observation` |
| runtime context | runtime context, byte-identical | `duplicate_component_evidence` |
| runtime context | runtime context, different order/reason/metadata/component semantics | `inconsistent_observation` |
| runtime fingerprint | runtime fingerprint, same or different digest | duplicate if identical, otherwise inconsistent |
| MCP fingerprint | MCP fingerprint, same or different digest | duplicate if identical, otherwise inconsistent |
| one kind | a different kind | accept if identity and present-integrity rules match |

Validation order is normative and independent of caller order: validate each
item's component, envelope, context shape, and limits first; group by component
ID and classify each complete same-kind multiset second; only after every group
passes, validate the global runtime-context semantic-order collection. Within a
same-kind multiset, distinct canonical bytes take precedence over exact
duplicates: every permutation of `A, A, B` returns
`inconsistent_observation`, while `A, A` and `A, A, A` return
`duplicate_component_evidence`. Two otherwise distinct context items with the
same semantic order return `inconsistent_observation`.

Across the complete runtime-context observed collection, semantic orders must
be exactly the empty set when `N = 0`; otherwise they must be unique and exactly
the integers zero through `N - 1`, inclusive. Gaps, duplicates between distinct
items, overflow, or a nonzero first order are `inconsistent_observation`.
Every runtime-context component must have present integrity and must satisfy the
closed producer-shape matrix:

| Selection reason | Component kind | Source scope | Required source locator |
| --- | --- | --- | --- |
| `workflow_runtime_profile_selected` | `agent_runtime` | `runtime` | a valid historical `runtime_profile/<exact-name>` locator, or `runtime_profile_name_sha256/<lowercase SHA-256>` exactly as produced by the merged GH-1732 remediation |
| `workflow_base_selected` | `workflow` | `runtime` | `workflow_source/central/<lowercase SHA-256>` |
| `workflow_repository_selected` | `workflow` | `repository` | exactly `WORKFLOW.md` |
| `workflow_document_effective` | `workflow` | `runtime` | exactly `workflow_document/effective` |
| `workflow_defaults_selected` | `workflow` | `runtime` | exactly `workflow_document/defaults` |
| `repo_memory_selected` | `memory` | `runtime` | `repo_memory/record-<canonical lowercase hyphenated UUID>` |

The snapshot validates the closed locator shape; the typed GH-1732 producer
remains responsible for binding a profile-name hash or central-path hash to its
preimage. `repo_memory_selected` additionally requires present memory metadata
whose record ID is the locator UUID. Metadata on any other reason, absent
integrity, a malformed hash or UUID, or any reason/kind/scope/locator mismatch
is `invalid_context_metadata`.

A not-observed domain has no collection by construction. An observed domain
with an empty collection is valid and proves an empty successful result. Any
failed domain aborts before coverage or stable identity is produced. A typed
GH-1733 expected probe failure remains an item inside an observed fingerprint
collection and is not a failed domain.

## Canonical Ordering

Canonical ordering is total and independent of input iteration:

1. component groups sort by exact `component_id.as_str().as_bytes()`;
2. evidence sorts by closed rank:
   repository inventory, runtime context, runtime fingerprint, MCP
   fingerprint;
3. evidence of the same rank sorts by its complete canonical evidence bytes;
4. capabilities use their existing ASC-001 exact wire order.

Fingerprint subject determines the fingerprint rank. A subject/payload mismatch
has already failed GH-1733 validation and is rechecked by the typed envelope
accessor.

Snapshot code never sorts arbitrary JSON arrays or reinterprets MCP schemas.
GH-1733 owns schema-context canonicalization and its verified digest. Map
insertion order is therefore normalized at the owning producer boundary, not
guessed again by the snapshot layer.

## Stable Projection

Define:

```text
frame(bytes)      = u64be(bytes.len) || bytes
count(n)          = u64be(n)
optional(bytes)   = 0x00 when absent
                  | 0x01 || frame(bytes) when present
component(c)      =
  frame(c.schema_version)
  || frame(c.component_id)
  || frame(c.kind)
  || frame(c.source.scope)
  || frame(c.source.locator)
  || frame(c.observation_class)
  || frame(c.selection_state)
  || optional(c.integrity.lowercase_hex)
  || count(c.capabilities.len)
  || frame(c.capability_0) || ...
  || frame(c.trust_level)
  || frame(c.freshness)
```

Every length and count conversion is checked. Enum values use their exact
snake_case UTF-8 wire spelling and every string or lowercase hex digest shown
above is framed. Capability order is the existing ASC-001 order. The Unix
executable tri-state is one raw tag byte: `0x00` for unavailable, `0x01` for
false, and `0x02` for true. Context optional memory metadata and its optional
evidence reference use the same raw `0x00`/`0x01` option tags.

The stable input is:

```text
"harness_agent_stack_snapshot_id_v0_1\0"
|| frame("agent-stack-snapshot/v0.1")
|| count(4)
|| frame("repository_inventory") || frame(coverage_state)
|| frame("runtime_context")       || frame(coverage_state)
|| frame("runtime_fingerprint")   || frame(coverage_state)
|| frame("mcp_fingerprint")       || frame(coverage_state)
|| count(entry_count)
|| frame(canonical_entry_0)
|| ...
```

Each canonical entry is:

```text
frame(component_id)
|| count(evidence_count)
|| frame(canonical_evidence_0)
|| ...
```

The complete evidence grammar is:

```text
repository_evidence =
  frame("repository_inventory")
  || component(component)
  || frame("repository_inventory/v0.1")
  || frame("regular_file" | "directory_presence")
  || executable_tag

runtime_context_evidence =
  frame("runtime_context")
  || component(component)
  || frame("runtime_context/v0.1")
  || u64be(semantic_order)
  || frame(selection_reason)
  || context_memory_metadata

context_memory_metadata =
  0x00
  | 0x01
    || frame(record_id)
    || optional(evidence_ref)
    || u64be(estimated_tokens)

runtime_fingerprint_evidence =
  frame("runtime_fingerprint")
  || component(envelope.component)
  || frame("agent_runtime")
  || frame(envelope.inner_schema_version)
  || frame(envelope.fingerprint_digest.lowercase_hex)

mcp_fingerprint_evidence =
  frame("mcp_fingerprint")
  || component(envelope.component)
  || frame("mcp_tool")
  || frame(envelope.inner_schema_version)
  || frame(envelope.fingerprint_digest.lowercase_hex)
```

`directory_presence` requires executable tag `0x00`. `regular_file` permits
all three executable tags. Runtime-context metadata is present exactly for
`repo_memory_selected` and absent for every other reason. The record ID and
evidence reference are exact bounded UTF-8 bytes; estimated tokens and
semantic order are raw fixed-width unsigned `u64` big-endian integers.
Runtime fingerprint inner schema must be exactly
`runtime-executable-fingerprint/v0.1`; MCP fingerprint inner schema must be
exactly `mcp-tool-fingerprint/v0.1`. Subject, payload, component kind, and
evidence kind must agree.

The closed evidence/component-kind matrix is:

| Evidence kind | Required fingerprint subject | Allowed component kinds |
| --- | --- | --- |
| `repository_inventory` | N/A | `instructions`, `skill`, `mcp_server`, `hook`, `memory`, `policy`, `workflow`, `validation` |
| `runtime_context` | N/A | `agent_runtime`, `workflow`, `memory` |
| `runtime_fingerprint` | `agent_runtime` | `agent_runtime` |
| `mcp_fingerprint` | `mcp_tool` | `mcp_tool` |

A group may combine different evidence kinds only when its component kind
appears in every applicable row and all exact ID/source/integrity consistency
rules also pass. Therefore the only type-level multi-kind intersections are
repository plus context for `memory` or `workflow`, and context plus runtime
fingerprint for `agent_runtime`; actual producer locators may narrow these
further. Both fingerprint kinds can never share a group, no three-kind
intersection exists, and every other combination is `inconsistent_observation`.

The outer `created_at`, optional run ID, stored `stable_id`, vector input
positions, raw diagnostics, durations, display paths, and current clock are not
read by this function. Exclusion is by a positive field whitelist, never by
recursive key deletion.

SHA-256 of these bytes becomes `AgentStackStableId`. The fixed empty vector
with all four coverage domains `observed` and zero entries is 251 bytes and has
expected digest:

```text
a70ef74bf084fba3e6d0d12daeebc09b24236ffe76d601a85f89cdc4f1106200
```

The independent non-empty vector frozen in `specs/GH1734/vectors.md` is 1,312
canonical bytes and contains repository plus runtime-context evidence. Its
portable test constructs the repository evidence from a typed ASC-002 entry
fixture and the context evidence from GH-1734 typed inputs. A Unix-only
integration test constructs the matching repository entry through the real
ASC-002 inventory; non-Unix inventory retains `unix_executable: None` and is
covered separately. The vector covers an empty capability list, executable
tag `0x02`, present integrity, canonical UUID memory identity, present context
metadata, and present evidence reference. Its expected digest is
`da375d4cf97e7b01281a18130dacc614706aec719b7611320aa1ccb6b846f49e`.
The vector document includes the full canonical input hex. Tests first build
the same snapshot through typed constructors and require byte-for-byte equality
with the literal, then independently decode and hash the literal; production
encoder output is not the digest oracle. Fingerprint integration instead
imports each complete valid typed envelope vector required by GH-1733 and
proves its verified digest enters the snapshot projection.

## Limits and Error Semantics

Inclusive v0.1 limits:

- component groups: 50,000;
- retained typed evidence bytes: 67,108,864 (64 MiB);
- canonical stable-projection bytes: 67,108,864 (64 MiB);
- context evidence reference: 4,096 UTF-8 bytes;
- run ID: 256 UTF-8 bytes.

The memory record ID is a fixed 36-byte canonical UUID newtype, not a
variable-length resource field. One evidence per kind is a consistency rule,
not a numeric resource limit; the fingerprint subject/component-kind matrix
means all four kinds are not required or expected to coexist in one group.

The constructor counts groups, retained bytes, and canonical bytes
incrementally with checked arithmetic before inserting each moved value. It
does not clone evidence. The exact retained-charge grammar is:

```text
retained_charge(repository) = byte_len(repository_evidence)
retained_charge(context)    = byte_len(runtime_context_evidence)
retained_charge(fingerprint)= byte_len(strict GH-1733 envelope JSON)
aggregate_retained_charge   = checked sum of every evidence charge
```

The first two lengths use the complete evidence grammar above, including every
repeated frame and tag exactly once. Fingerprint charge includes the full
validated strict envelope wire, including component, description, annotations,
schemas, environment facts, and safe diagnostics; it is not the smaller stable
projection. GH-1734 adds a crate-private checked-length method beside the
GH-1733 envelope serializer. That method runs the same strict serializer into
a count-only writer, emits no second wire buffer, and is tested equal to the
actual serialized byte length for every complete GH-1733 vector and optional
branch. A serializer failure is `invalid_fingerprint`, never zero bytes.

Both aggregate counters use the same checked transition: a prior charge of
67,108,863 plus one succeeds at 67,108,864; plus two returns the corresponding
limit error without updating the counter. Literal arithmetic tests pin those
exact/+1 transitions without allocating a 64 MiB fixture, while integration
tests prove actual evidence lengths feed the counter. Existing per-envelope
upstream limits remain authoritative in addition to this aggregate limit.

Closed error categories are:

- `invalid_component`
- `invalid_fingerprint`
- `invalid_context_metadata`
- `invalid_run_id`
- `duplicate_component_evidence`
- `inconsistent_observation`
- `observation_failed`
- `component_limit_exceeded`
- `context_field_limit_exceeded`
- `retained_evidence_bytes_limit_exceeded`
- `canonical_bytes_limit_exceeded`
- `canonical_length_overflow`

`AgentStackProducerFailureKind` is closed to `source_unavailable`,
`invalid_evidence`, `limit_exceeded`, and `interrupted`. It carries no domain.
`observation_failed` reports only the input slot's closed domain and that safe
failure kind. GH-1733 expected probe outcomes do not use this failure type.

Errors may include a safe component ID and closed evidence kind when already
validated. They never include raw file contents, memory payload, environment
values, MCP descriptions/schemas, absolute sensitive paths, probe output, or
generic encoder diagnostics.

No snapshot object or stable ID is returned with an error. Hashing an empty
buffer on failure is prohibited.

## Internal Value and ASC-006 Boundary

ASC-005 does not derive public `Serialize` or `Deserialize`, expose
`from_json`, or provide a snapshot wire encoder. Read-only accessors expose
typed fields to in-process ASC-007 consumers. This prevents complete GH-1733
envelopes, including potentially secret-bearing MCP annotations or schemas,
from becoming an unredacted public snapshot format.

ASC-006 must:

- define the redacted versioned wire projection and its relationship to
  upstream fingerprint digests;
- add strict import and unknown-field rejection;
- enforce deny-by-default redaction before any serialized output;
- rerun component, envelope, duplicate, consistency, coverage, and limit
  checks;
- define how an importer verifies redacted evidence without pretending it
  possesses omitted fingerprint payloads;
- recompute every verifiable stable projection and reject mismatches; and
- preserve the exact stable-ID algorithm and limits defined here.

Typed construction already enforces its own invariants; those are not deferred
to ASC-006.

## Producer Adapters

### Repository Inventory

Core adds the narrow consuming API
`pub(crate) fn into_entries(self) -> Vec<AgentStackInventoryEntry>` to
`AgentStackInventory` and moves each entry directly; no public iterator or
clone-based adapter is introduced. The conversion retains the validated
component and exact `AgentStackEntryClass`. Because `stack/snapshot/tests.rs`
is a sibling of the inventory module, `AgentStackInventoryEntry` also adds one
`#[cfg(test)] pub(super)` typed fixture factory in `stack/inventory/mod.rs`.
It accepts a validated component plus an explicit `AgentStackEntryClass`, is
absent from production builds, and is the portable construction path for the
literal `0x02` vector. The official repository helper accepts
`Result<AgentStackInventory, AgentStackInventoryError>` and maps every current
error kind exhaustively: `LimitExceeded` becomes `limit_exceeded`;
`InvalidOptions`, `ConfigParse`, `ConfiguredSourceInvalid`, and
`ComponentValidation` become `invalid_evidence`; every I/O, race, escape,
missing-source, cycle, metadata, or unsupported-entry kind becomes
`source_unavailable`. Adding an inventory error variant breaks the exhaustive
match. On Unix, a real-inventory regular-file executable toggle has a dedicated
integration test. On non-Unix, the real inventory test requires
`unix_executable: None`; portable typed-fixture tests cover the `0x00`, `0x01`,
and `0x02` canonical tags on every target. A file-to-directory-presence change
has a dedicated integration test on every target.

### Runtime Context

After the GH-1732 remediation is merged, GH-1734 changes
`ContextProvenanceEntry.reason` from `&'static str` to the single closed core
enum and preserves the six exact serialized strings. It also introduces a
closed `ContextProvenanceBuildErrorKind` at the existing `anyhow` boundary.
The crate-visible conversion:

1. carries the already-core-owned reason without a duplicate mapping;
2. checked-converts `usize` order and estimated tokens to `u64`;
3. validates present integrity for every context entry;
4. validates the memory kind/scope/canonical-UUID locator/metadata identity
   conjunction;
5. converts the bounded record ID and evidence reference without generic JSON;
   and
6. maps `Result<ContextProvenance, ContextProvenanceBuildError>` directly to
   `Observed` or `Failed`, never `NotObserved`.

The closed context build errors map validation/serialization/checked-conversion
failures to `invalid_evidence`, resource bounds to `limit_exceeded`, explicit
cancellation to `interrupted`, and selected-source I/O/unavailability to
`source_unavailable`. Contract tests inject every category.

This change supplies an adapter and contract tests only. It does not begin
automatic snapshot collection. Real fixtures change a selected memory record's
agent-visible `created_at` and `use_count`, prove that GH-1732 changes component
integrity and therefore stable ID, and separately prove that changing outer
snapshot `created_at` preserves stable ID.

### Runtime and MCP Fingerprints

Core conversion accepts a validated GH-1733 envelope by value. It validates
again, reads the subject and inner schema from the closed payload, and carries
the full envelope for later structural diff while using the verified
fingerprint digest in the stable projection. It never reparses schema JSON or
substitutes component integrity for payload identity.

The runtime wrapper's typed `observed` constructor accepts zero or more
validated runtime-subject envelopes, including the successful empty case. The
harness-agents runtime producer adapter accepts the actual
`Result<AgentStackFingerprintEnvelope, RuntimeFingerprintProduceError>` and
maps `Ok` to a one-envelope runtime wrapper. It exhaustively maps
input/component/schema/digest contract errors to
`invalid_evidence`, resource-limit errors to `limit_exceeded`, explicit caller
cancellation to `interrupted`, and OS/probe/containment/cleanup/timeout
unavailability to `source_unavailable`. Expected probe failures that GH-1733
represents inside a valid envelope remain `Observed`. An added producer-error
variant breaks compilation until classified.

GH-1733 defines MCP envelope construction/strict parsing but no MCP network
collector. The core MCP helper therefore accepts the actual
`Result<Vec<AgentStackFingerprintEnvelope>, AgentStackFingerprintError>`,
requires every successful envelope to have subject `mcp_tool`, and exhaustively
maps contract errors to `invalid_evidence` or `limit_exceeded`. A later network
collector owns its transport-error mapping and cannot claim ASC-005 compliance
without passing an explicit `Failed` kind. `NotObserved` is available only
through an explicit no-producer-attempt constructor and accepts no `Result`.

## Minimal Identity Comparison

`AgentStackSnapshot::compare_identity` first requires the same snapshot schema
and exact coverage profile. Different coverage returns
`incompatible_coverage`. Equal stable IDs return `same`.

For unequal IDs, the helper may return `identity_discontinuity` only when:

1. all but one canonical component group match exactly;
2. there is exactly one unmatched before group and one unmatched after group;
3. both groups contain the same evidence-kind set; and
4. after removing only component ID, source scope, and source locator from
   each full component projection, their ordered evidence bytes are exactly
   equal.

The result carries the before and after component IDs only. Zero, multiple, or
ambiguous candidates, or any simultaneous semantic change, returns
`different`. The helper does not call a path change a rename, does not match by
display name, and emits no added/removed/modified field facts. ASC-007 builds
the full structural diff on the typed snapshots.

## Test Plan

### Fixed and Round-Trip Construction

- Build a successful empty snapshot for each coverage combination.
- Construct every component kind, source scope, observation, selection, trust,
  freshness, and capability through typed evidence.
- Cover repository, context, runtime-fingerprint, and MCP-fingerprint evidence.
- Construct the non-empty literal vector through typed repository/context
  inputs, require exact bytes, and independently hash both literal vectors.
- Feed GH-1733's complete valid runtime and MCP vectors through their typed
  parsers and snapshot conversion.

### Invariance

- Reverse component input order and evidence input order.
- Vary map insertion order only inside already canonical upstream evidence.
- Change only outer observation `created_at` and run ID.
- Rebuild identical facts after an interrupted prior attempt.
- Preserve stable ID for upstream MCP object/set reorder that GH-1733 defines
  as non-semantic.

### Sensitivity and Invalid Couplings

Each independently valid semantic change alters the stable ID: a complete
component identity/source change, observation, selection, integrity,
capability, trust, freshness, repository class/mode, context reason/metadata,
valid two-entry semantic-order swap, verified fingerprint payload/digest, or
coverage/component/evidence presence.

Mutations that break a coupled invariant return a typed error and no stable ID:
component ID without its derived kind/source change; fingerprint subject,
payload kind, fixed inner schema, source binding, or digest changed alone;
one context order changed into a gap/duplicate; context integrity removed; or
memory kind/scope/record locator/metadata made inconsistent.

### Conflict and Failure

- Same ID with different kind/source after malformed test construction.
- Same ID with two different present integrity digests.
- The complete same-kind compatibility matrix.
- Exact context duplicate before global order validation; two distinct context
  items with the same order; input-order reversal; gapped, nonzero-first, and
  overflowed context semantic order.
- Every row of the six-reason context producer-shape matrix plus one-field
  kind, scope, locator, hash/UUID spelling, integrity, and metadata mutations.
- Each of four domain failures versus successful empty observation versus
  not-observed.
- Every producer `Result` error category maps to `Failed`; explicit
  no-attempt is the only path to `NotObserved`.
- Invalid/blank/over-limit run ID.
- The subject/component-kind compatibility matrix rejects impossible
  cross-kind groups; every reachable different-kind combination is tested.
  Same-kind repeats use the duplicate/inconsistent rules.
- Every exact limit and limit-plus-one, including count-only 64 MiB arithmetic
  vectors and aggregate retained bytes across components.
- Checked-length overflow through a test seam.
- Canonicalization error returns no snapshot and never hashes empty bytes.

### Integration

- The repository helper consumes a real `AgentStackInventory` through
  `into_entries`. On Unix, real-inventory executable-bit changes alter stable
  identity; on non-Unix, real inventory retains `unix_executable: None` and
  the typed fixture proves executable-tag sensitivity.
- Runtime-context caller-vector reorder is invariant; swapping the explicit
  orders of two otherwise valid entries changes identity, while a one-entry
  gap/duplicate mutation fails.
- Selected memory `created_at`/`use_count` changes component integrity and
  stable identity while outer snapshot time does not.
- A validated GH-1733 expected failure is accepted and differs from success.
- Runtime and MCP payload changes alter identity while upstream
  representation-only normalization does not.
- Existing `cargo test -p harness-core stack` and server
  context-provenance tests remain green.
- Minimal identity comparison reports one unambiguous source discontinuity,
  rejects ambiguity, and returns incompatible for unequal coverage.

## Verification

During implementation:

Before accepting any focused filtered command in this section, run the same
filter with `-- --list` and require at least one matching test. Zero matches
fail verification.

```text
cargo test -p harness-core stack::snapshot::tests
cargo test -p harness-server context_provenance
cargo check -p harness-core --all-targets
cargo check -p harness-server --all-targets
```

Before commit and push:

```text
cargo fmt --all
cargo fmt --all -- --check
cargo test --workspace
cargo test -p harness-core
cargo test -p harness-server context_provenance
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

GH-1734 adds no persistence-specific test, but this does not waive the full
workspace test gate. Always run `cargo test --workspace`. Run
PostgreSQL-dependent suites with an isolated `HARNESS_DATABASE_URL`; when none
is available, run the DB-less pre-push path, record only the explicit
PostgreSQL suites as deferred, and require current-head CI or a later isolated
database run to pass them. DB-less success is not evidence that those suites
passed.

## Alternatives Considered

- Hash only `Vec<AgentStackComponent>`: rejected because executable mode,
  context ordering, and fingerprint payload changes live outside components.
- Hash serialized snapshot JSON: rejected because it includes volatile
  observation metadata and depends on wire representation.
- Keep only the fingerprint digest: rejected because GH-1733 intentionally
  excludes the outer component from that digest.
- Recursively remove every `created_at` key: rejected because a timestamp may
  be agent-visible behavior already covered by an upstream typed digest.
- Last-write-wins for duplicate component IDs: rejected because it hides
  observation conflicts and makes input order authoritative.
- Flatten multiple observations into the strongest trust level: rejected
  because trust, freshness, and observation changes must remain auditable.
- Recanonicalize MCP schemas: rejected because GH-1733 owns the
  context-sensitive schema contract and blanket sorting is unsafe.
- Add the import parser now: rejected because ASC-006 owns untrusted snapshot
  validation and redaction.

## Risks

- Logic: omitting an out-of-component producer fact creates a false stable ID;
  evidence-specific sensitivity tests prevent this.
- Logic: including volatile metadata creates false drift; the positive
  projection whitelist and metadata invariance tests prevent it.
- Security: arbitrary payloads or diagnostics could leak through snapshots;
  only closed typed evidence is accepted, with no raw generic extension.
- Compatibility: changing framing changes every ID; schema version and fixed
  vectors make such a change explicit.
- Performance: large inventories require bounded streaming canonicalization;
  count and byte limits prevent unbounded allocation.
- Maintenance: producer contracts may add a field; exhaustive typed adapters
  and evidence tests must decide whether it enters their upstream digest.
