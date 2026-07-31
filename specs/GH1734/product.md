# Product Spec

## Linked Issue

GH-1734

complexity: high

## User Problem

Harness has typed evidence for repository-observed Agent Stack components and
runtime-selected context, and GH-1733 defines typed runtime and MCP
fingerprints. It still has no aggregate value that answers whether two
observations describe the same behavior-affecting stack.

Hashing a `Vec<AgentStackComponent>` is insufficient. Repository executable
mode and directory-presence facts live outside the component, runtime-context
selection reason and order live outside the component, and runtime/MCP
fingerprint payloads deliberately use a digest separate from component
integrity. A snapshot that omits those facts can retain the same ID after a
behavior change.

The aggregate identity must also distinguish semantic evidence changes from
incidental observation metadata. A new collection time or runtime job ID must
not create drift, while a change to selected order, trust, freshness,
executable mode, or a verified fingerprint must.

## Goals

- Define one versioned, immutable Agent Stack snapshot value and a distinct
  stable-ID type.
- Aggregate only validated typed evidence from repository inventory,
  runtime-context provenance, and GH-1733 fingerprints.
- Preserve multiple compatible observations of one canonical component
  identity without last-write-wins behavior.
- Canonicalize representation-only ordering while retaining explicit semantic
  ordering as a behavior fact.
- Keep collection time and run identity outside the stable-ID projection.
- Fail closed on producer errors, duplicate evidence, inconsistent component
  identity, or canonicalization limits.

## Non-Goals

- Discovering repository files, loading runtime context, starting an agent,
  connecting to MCP, or running a fingerprint probe.
- Serializing, validating, redacting, or importing a public snapshot wire
  format; ASC-006 owns that boundary.
- Computing full structural change facts or promotion verdicts. ASC-005 owns
  only the minimal identity-continuity result in B-015; ASC-007 owns typed
  field-level diffs.
- Extracting declared, granted, or observed capabilities.
- Signing, attesting, or claiming that a SHA-256 digest proves execution.
- Adding a CLI, HTTP endpoint, database schema, automatic persistence, or
  dashboard.
- Treating absent coverage as proof that an unobserved source did not exist.

## User-Visible Behavior

1. **B-001:** Every successfully constructed snapshot has schema version
   `agent-stack-snapshot/v0.1`, a distinct lowercase SHA-256 `stable_id`,
   canonical entries, a closed coverage profile, and separate observation
   metadata. The stable ID is a deterministic content identifier, not a
   signature, attestation, trust proof, or run identifier.
2. **B-002:** Snapshot inputs are closed typed evidence:
   `repository_inventory`, `runtime_context`, `runtime_fingerprint`, or
   `mcp_fingerprint`. Repository evidence consumes the complete
   `AgentStackInventoryEntry`, not only its component. Runtime-context
   evidence carries its validated component, explicit semantic order, closed
   selection reason, and closed optional memory metadata as typed fields.
   GH-1734 owns conversion of the existing private context-reason strings to
   that closed enum after the GH-1732 remediation is merged.
   Fingerprint evidence consumes a validated GH-1733 envelope. There is no
   public `serde_json::Value`, generic serializable, arbitrary map, free-form
   evidence-kind string, or `Any` entry point.
3. **B-003:** Entries are grouped by exact ASC-001 component ID. Every evidence
   item in a group must contain the same validated component ID, kind, source
   scope, and source locator. Multiple repository/runtime/runner observations
   may share that identity and remain independently visible. Conflicting
   present integrity digests fail as `inconsistent_observation`; no source is
   silently preferred. Each component may have at most one evidence item of
   each closed kind. A second item of the same kind with identical canonical
   bytes is `duplicate_component_evidence`; one with different bytes is
   `inconsistent_observation`. Runtime-context semantic orders are globally
   unique and contiguous from zero through `N - 1`. There is no last-write-
   wins or implicit reordering.
4. **B-004:** The stable projection explicitly includes every semantic
   ASC-001 component field: component schema, component ID, kind, source scope
   and locator, observation class, selection state, integrity presence/value,
   canonical capabilities, trust level, and freshness. Changes to those
   fields change the stable ID. The projection never serializes the entire
   snapshot and then removes fields by name.
5. **B-005:** Evidence-specific behavior facts also enter the stable
   projection:
   - repository regular-file versus directory-presence class and the exact
     Unix executable tri-state;
   - runtime-context semantic order, closed selection reason, and the exact
     typed optional memory metadata fields;
   - fingerprint subject, inner schema version, and verified
     `fingerprint_digest`.
   Therefore chmod, source-selection ordering, runtime version/environment/
   failure, MCP description/annotations/schema, and other upstream
   fingerprint changes cannot be invisible merely because component identity
   stayed constant.
6. **B-006:** Representation order is not semantic. Component groups sort by
   exact UTF-8 component-ID bytes. Evidence sorts by a closed evidence-kind
   rank and its canonical bytes. Capabilities retain ASC-001 canonical order.
   Reordering caller vectors or map insertion order does not change the stable
   ID. An explicit runtime-context `semantic_order` is a fact, so changing it
   does change the ID.
7. **B-007:** Construction consumes exactly four closed per-domain observation
   inputs, each `not_observed`, `observed(typed collection)`, or
   `failed(closed producer failure)`. The domains are:
   `repository_inventory`, `runtime_context`, `runtime_fingerprint`, and
   `mcp_fingerprint`. Coverage is derived from those inputs and is part of the
   stable identity because equal visible entries under different observation
   scopes are not comparable evidence. An observed domain may legitimately
   contain zero entries. A not-observed domain contains no collection. Any
   failed domain returns `observation_failed` and produces no snapshot.
   A producer failure contains only a closed failure kind; its domain is
   derived from the input slot, so contradictory domain metadata cannot be
   supplied. Official conversion helpers consume each producer `Result`
   directly and may not rewrite `Err` as `not_observed`. The public domain-
   observation value is opaque; its state enum is private. `not_observed` is
   available only through an explicitly named no-producer-attempt constructor,
   not a public enum variant.
8. **B-008:** Snapshot observation metadata contains the collection
   `created_at` and an optional nonblank bounded run ID. Those two outer
   fields are retained in typed observation metadata but excluded from the
   stable projection. Changing only them preserves the stable ID. This
   exclusion is not recursive: a `created_at` or counter already present in
   agent-visible runtime-context content remains covered by upstream component
   integrity.
9. **B-009:** Stable-ID hashing is domain separated with
   `harness_agent_stack_snapshot_id_v0_1\0`. The schema, coverage records,
   entry count, and each canonical entry use fixed unsigned `u64`
   big-endian count or byte-length framing. No platform-native integer,
   filesystem path display, JSON object insertion order, locale, or current
   clock enters the digest.
10. **B-010:** Paths are never canonicalized again at the snapshot layer.
    Snapshot construction accepts only validated ASC-001 source locators and
    preserves their exact canonical bytes. It performs no lossy UTF-8
    conversion, filesystem canonicalization, symlink collapse, blanket
    case-folding, or display-path fallback.
11. **B-011:** Snapshot construction is all-or-nothing. A real inventory,
    context, or fingerprint producer error returns a typed non-secret error
    and no stable ID. A GH-1733 expected probe-failure envelope remains valid
    behavior evidence and changes the ID according to its verified payload.
    Canonicalization or checked-length failure cannot fall back to empty bytes
    or an empty digest.
12. **B-012:** A successful empty snapshot is allowed and is distinct from
    failed or unobserved collection through its coverage profile. Rebuilding
    the same typed facts and coverage produces the same stable ID. Adding,
    removing, or modifying any included fact changes it. Fixed independent
    framing vectors and tamper-sensitivity fixtures prove both properties.
13. **B-013:** Construction is bounded before snapshot-owned allocation: at
    most 50,000 component groups, with one evidence record per closed kind and
    subject/kind compatibility further restricting reachable combinations.
    This derived cardinality is not a separate resource limit. The aggregate
    has independent 64 MiB limits for retained typed evidence and canonical
    stable-projection bytes.
    Repository/context retained charge is their complete canonical evidence
    byte length; fingerprint retained charge is the complete strict GH-1733
    envelope wire length measured without producing a second wire buffer.
    Inputs move into the snapshot without cloning. Exact limits succeed;
    limit-plus-one fails with a closed limit category. All counters and length
    conversions use checked arithmetic.
14. **B-014:** ASC-005 remains an internal typed library aggregation boundary.
    It exposes no public snapshot `Serialize`/`Deserialize` implementation,
    wire encoder, automatic collection, or persistence. Full validated
    fingerprint envelopes remain internal typed evidence for ASC-007; they
    cannot leak MCP descriptions, annotations, schemas, environment facts, or
    diagnostic text through an ASC-005 wire format. ASC-006 must define the
    redacted public projection and its digest-verification semantics.
15. **B-015:** ASC-005 provides the minimal comparison required by ASC-001:
    `same`, `different`, `incompatible_coverage`, or an unambiguous
    `identity_discontinuity`. A discontinuity is reported only when coverage
    matches, exactly one before group and one after group are otherwise
    canonical-byte equal after removing component ID/source identity, and all
    other groups are equal. Multiple candidates or any other semantic change
    returns `different`; the helper never guesses a rename. ASC-007 owns
    added/removed/modified and field-level change facts.
16. **B-016:** Implementation cannot start until the GH-1732 remediation spec
    and GH-1733 spec are approved, both upstream issues are
    `ready_to_implement`, both implementations are merged, and GH-1734 itself
    is approved and `ready_to_implement`. PR #1859 is not an accepted
    substitute for the GH-1733 dependency.

## Acceptance Criteria

- [ ] Public Rust types represent the snapshot, stable ID, coverage,
      observation metadata, grouped entries, and all closed evidence variants
      without generic JSON or untyped extension fields.
- [ ] Repository construction proves that executable-bit and
      directory-presence changes affect the stable ID.
- [ ] Runtime-context construction proves that caller-vector reordering is
      invariant while explicit semantic-order, selection-reason, typed memory
      metadata, or upstream component-integrity changes affect the ID.
- [ ] Runtime-context construction retains typed optional memory metadata;
      changing only a selected memory record's agent-visible `created_at` or
      `use_count` changes component integrity and the stable ID, while changing
      only snapshot observation `created_at` does not.
- [ ] Every runtime-context evidence has present component integrity.
      `repo_memory_selected` additionally requires kind `memory`, runtime scope,
      an exact `repo_memory/record-<canonical UUID>` locator, and a metadata
      record ID equal to that locator suffix; absent integrity or mismatched
      identity fails without a snapshot.
- [ ] Runtime and MCP construction accepts only validated GH-1733 envelopes;
      fingerprint payload changes affect the ID and schema/object
      representation noise already canonicalized upstream does not.
- [ ] Component input order, evidence input order, and observation
      `created_at`/run-ID changes preserve the stable ID.
- [ ] Every independently valid component/evidence semantic change has a
      sensitivity fixture that changes the ID. A one-field mutation that
      breaks derived identity, fixed schema, subject/payload, semantic-order,
      or memory-identity coupling instead fails typed and returns no ID.
- [ ] The evidence compatibility matrix, global context-order uniqueness and
      contiguity, conflicting present integrity, every per-domain failure/
      empty/not-observed state, and every limit-plus-one case fail or succeed
      exactly as specified.
- [ ] An independently calculated empty-snapshot vector pins the domain and
      framing. A literal non-empty repository/context vector is constructible
      through the closed typed inputs and pins representative option/tag
      branches. Branch-matrix tests cover the remaining executable, coverage,
      metadata, and evidence-reference tags, while fingerprint integration
      reuses GH-1733's complete valid typed vectors. Production encoder output
      is not used as the oracle for either literal snapshot vector.
- [ ] Minimal comparison reports only unambiguous identity discontinuity and
      leaves all structural detail to ASC-007.
- [ ] Existing Agent Stack component, inventory, context-provenance, and
      fingerprint contracts are reused rather than copied or weakened.
- [ ] No public snapshot serialization, external dependency, database
      migration, CLI/API route, or automatic runtime consumer is introduced.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-007 and B-012. Empty observed domains are explicit; producer failure is not empty success. |
| Error and failure paths | Covered by B-003, B-011, and B-013. |
| Authorization / permission | The snapshot grants no authority and makes no attestation claim. |
| Concurrency / race / ordering | Upstream producers own observation races; B-003 rejects inconsistent same-snapshot evidence and B-006 removes representation-order drift. |
| Retry / repetition / idempotency | Covered by B-006, B-008, and B-012. |
| Illegal state transitions | N/A. This is an immutable value constructor; invalid evidence combinations fail before construction. |
| Compatibility / migration | Covered by B-001, B-014, and B-016. Import compatibility belongs to ASC-006. |
| Degradation / fallback | Covered by B-007 and B-011; no warning-only partial snapshot exists. |
| Evidence and audit integrity | Covered by B-002 through B-016. |
| Cancellation / interruption / partial completion | A producer interruption is an error and yields no snapshot. |

## Edge Cases

- A repository inventory succeeds with no matching files.
- The same component has repository-observed and runtime-observed evidence.
- Two observations share an ID but have different present integrity digests.
- Two exact evidence records are submitted in opposite positions.
- A runtime-context vector is reordered without changing its semantic ranks.
- A selected memory entry changes only an agent-visible timestamp or use
  count covered by its upstream digest.
- A selected memory entry omits integrity or its record ID differs from the
  canonical locator suffix.
- A fingerprint records an expected timeout or version-parse failure.
- An MCP schema changes only object insertion order versus changing an ordered
  tuple location.
- Coverage changes from `not_observed` to an observed empty domain.
- Snapshot collection time and run ID change between retries.
- An ASC-001 path is case-distinct except for its already canonical Windows
  drive prefix.
- Canonical projection size or a checked `u64` conversion exceeds its limit.

## Rollout Notes

The first implementation exposes an internal typed value, read-only accessors,
stable-ID calculation, and minimal identity comparison only. It does not
serialize or accept snapshot JSON. ASC-006 must define a deny-by-default
redacted wire format, strict import validation, unknown-field rejection, and
the relationship between redacted evidence, upstream fingerprint digests, and
the stable projection frozen here.
