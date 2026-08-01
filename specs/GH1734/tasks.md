# Task Plan

## Linked Issue

GH-1734

## Execution Gate

Do not implement this plan until the GH-1732 remediation and GH-1733 specs are
approved, GH-1732, GH-1733, and GH-1734 are all `ready_to_implement`, and both
upstream implementations are merged. Work only on the implementation branch
selected after those gates; do not use PR #1859 as an unapproved dependency.

## Tasks

- [ ] `SP1734-T1` Owner: core snapshot model worker | Dependencies: approved GH-1732 remediation, GH-1733, and GH-1734 specs; all three issues ready; both upstream implementations merged | Done when: the closed model and all construction invariants are implemented | Verify: focused model tests and core check below | Covers: B-001, B-002, B-003, B-007, B-008, B-013, B-014, B-016
- [ ] `SP1734-T2` Owner: canonical identity worker | Dependencies: SP1734-T1 | Done when: framed bounded stable identity and independent vectors are implemented | Verify: focused canonical and stable-ID tests below | Covers: B-004, B-005, B-006, B-009, B-010, B-012, B-013
- [ ] `SP1734-T3` Owner: repository adapter worker | Dependencies: SP1734-T1 and SP1734-T2 | Done when: complete inventory-entry facts map without loss | Verify: focused repository and inventory tests below | Covers: B-002, B-005, B-010, B-011
- [ ] `SP1734-T4` Owner: context adapter worker | Dependencies: SP1734-T1, SP1734-T2, and the merged GH-1732 remediation/component-integrity contract | Done when: GH-1734's closed reason/build-error types and every typed context fact map exhaustively without generic JSON | Verify: focused context and server checks below | Covers: B-002, B-005, B-006, B-008, B-011, B-016
- [ ] `SP1734-T5` Owner: fingerprint adapter worker | Dependencies: SP1734-T1, SP1734-T2, and the implemented GH-1733 core envelope | Done when: validated runtime/MCP envelopes, no-envelope producer failures, and strict-wire retained lengths map without recanonicalization | Verify: focused snapshot and fingerprint tests below | Covers: B-002, B-005, B-006, B-011, B-013, B-014
- [ ] `SP1734-T6` Owner: adversarial verification worker | Dependencies: SP1734-T1 through SP1734-T5 | Done when: invariance, sensitivity, conflict, limit, and minimal-comparison matrices pass | Verify: full core stack and context tests below | Covers: B-001 through B-016
- [ ] `SP1734-T7` Owner: integration and release verifier | Dependencies: SP1734-T6 | Done when: exact-head library-only scope and release gates pass | Verify: fmt, package, workspace, and clippy commands below | Covers: B-012 through B-016

### SP1734-T1 — Add the closed snapshot model

- Done when:
  - Closed snapshot, stable-ID, coverage, observation, grouped-entry,
    evidence, and error types exist in split files.
  - All fields are private and there is no generic
    JSON/serializable/`Any` entry point.
  - Grouping rejects exact duplicates, inconsistent identity or present
    integrity, coverage mismatch, invalid run IDs, and limits.
  - Item validation precedes group duplicate/conflict classification, which
    precedes global semantic-order validation; exact duplicate context evidence
    has a deterministic error independent of input order.
  - A failure carries no domain; the constructor derives it from the slot.
    The observation value is opaque, and `NotObserved` is available only
    through `not_observed_without_attempt`.
- Verify:
  - `cargo test -p harness-core stack_snapshot::model`
  - `cargo check -p harness-core --all-targets`

### SP1734-T2 — Implement canonical stable identity

- Done when:
  - The positive-whitelist framed projection, closed ordering, checked
    counts/lengths, bounded streaming hash, and distinct stable-ID newtype are
    implemented.
  - The independent empty vector
    `a70ef74bf084fba3e6d0d12daeebc09b24236ffe76d601a85f89cdc4f1106200`
    and the constructible repository/context vector
    `da375d4cf97e7b01281a18130dacc614706aec719b7611320aa1ccb6b846f49e`
    are pinned.
  - Outer observation time and run ID never enter the hash.
- Verify:
  - `cargo test -p harness-core stack_snapshot::canonical`
  - `cargo test -p harness-core stack_snapshot::stable_id`

### SP1734-T3 — Map complete repository inventory evidence

- Done when:
  - `AgentStackInventory` retains its existing public read-only `entries(&self)`
    accessor and adds exactly one crate-visible consuming `into_entries(self)`
    API for the adapter; typed conversion moves every complete
    `AgentStackInventoryEntry` without cloning.
  - Executable tri-state and directory presence enter the stable projection.
  - Every `AgentStackInventoryErrorKind` maps exhaustively to one closed
    producer-failure kind; inventory failure yields no snapshot.
- Verify:
  - `cargo test -p harness-core stack_snapshot::repository`
  - `cargo test -p harness-core stack::inventory`

### SP1734-T4 — Map typed runtime-context evidence

- Done when:
  - The server replaces private reason strings with the single core-owned
    GH-1734 enum and preserves its exact serialized spellings; no duplicate
    server enum or mapping exists.
  - It checked-converts semantic order and complete typed memory metadata,
    requires component integrity, and passes the contribution without
    `serde_json::Value`.
  - All six closed selection reasons enforce the exact producer kind, scope,
    and locator matrix, including the GH-1732 runtime-profile hash fallback.
  - Closed build errors map from the actual producer `Result` to `Failed`.
  - Tests require canonical UUID memory identity and distinguish representation
    reorder, valid order swap, and invalid one-field gap/duplicate.
  - No automatic snapshot collection is added.
- Verify:
  - `cargo test -p harness-server context_provenance`
  - `cargo test -p harness-core stack_snapshot::context`
  - `cargo check -p harness-server --all-targets`

### SP1734-T5 — Map validated runtime and MCP fingerprints

- Done when:
  - Only validated runtime/MCP envelopes are accepted.
  - Subject, inner schema, fingerprint digest, and full component semantics
    enter the stable projection.
  - Expected probe failures are valid evidence.
  - No-envelope producer errors map exhaustively to `Failed`.
  - GH-1734's count-only strict-envelope wire method matches actual GH-1733
    serialization for every complete vector and optional branch.
  - No schema recanonicalization or `harness-agents` reverse dependency is
    introduced.
- Verify:
  - `cargo test -p harness-core stack_snapshot::fingerprint`
  - `cargo test -p harness-core fingerprint`
  - `cargo test -p harness-agents runtime_fingerprint`
  - `cargo check -p harness-agents --all-targets`

### SP1734-T6 — Prove adversarial invariants

- Done when:
  - Every valid semantic change has a sensitivity fixture; every invalid
    coupled-field mutation fails without an ID.
  - Vector-order, outer-time, and run-ID invariance are proven.
  - Coverage, duplicate, conflict, producer-error, exact-limit,
    limit-plus-one, and overflow seams fail typed.
  - Exact context duplicates are classified before global order duplicates;
    distinct entries sharing one order remain inconsistent and reversing input
    order cannot change either error.
  - Every unique permutation of a mixed same-kind `A, A, B` multiset is
    `inconsistent_observation`; all-identical multisets remain
    `duplicate_component_evidence`.
  - Every context-reason matrix row has a positive producer fixture and
    kind/scope/locator one-field negative fixtures.
  - The subject/component-kind matrix covers every reachable different-kind
    combination without treating a derived evidence count as a resource limit.
  - Minimal comparison distinguishes equal, different, incompatible coverage,
    and only unambiguous identity discontinuity.
  - Every new file remains below 800 lines and no test is weakened.
- Verify:
  - `cargo test -p harness-core stack_snapshot`
  - `cargo test -p harness-core stack`
  - `cargo test -p harness-server context_provenance`
  - `cargo check --workspace --all-targets`

### SP1734-T7 — Close exact-head release gates

- Done when:
  - Changes remain library-only with no CLI, API, persistence, or automatic
    producer invocation.
  - Formatting, full workspace tests, package-focused tests, workspace check,
    and workspace clippy pass on the exact implementation commit.
  - PostgreSQL-dependent suites pass with an isolated database or are
    explicitly deferred under the repository policy to current-head CI; a
    DB-less pre-push success is not recorded as PostgreSQL-suite success.
  - The implementation PR references GH-1734 and records that ASC-006 still
    owns untrusted import and redaction.
- Verify:
  - `cargo fmt --all`
  - `cargo fmt --all -- --check`
  - `cargo test --workspace`
  - `cargo test -p harness-core`
  - `cargo test -p harness-agents runtime_fingerprint`
  - `cargo test -p harness-server context_provenance`
  - `cargo check --workspace --all-targets`
  - `cargo clippy --workspace --all-targets -- -D warnings`

## Stop Conditions

- The GH-1732 remediation, GH-1733, or GH-1734 lacks an approved spec or
  `ready_to_implement`, or either upstream implementation is not merged.
- The proposed implementation requires accepting generic JSON, silently
  dropping producer failures, or treating absent coverage as success.
- A producer fact cannot be mapped without guessing whether it is semantic.
- The stable-ID framing or fixed vectors would change after implementation
  begins without a new approved schema version.
- A new CLI/API/persistence consumer is required; route that work to the
  owning later issue instead.

## Handoff

```yaml
handoff:
  mode: fixflow
  artifacts:
    - specs/GH1734/product.md
    - specs/GH1734/tech.md
    - specs/GH1734/tasks.md
    - specs/GH1734/vectors.md
  runtime_pinning_snapshot: required-before-implementation
  verification_owner: integration and release verifier
  stop_conditions:
    - GH1732 remediation, GH1733, and GH1734 must be approved and ready_to_implement
    - GH1732 remediation and GH1733 implementations must be merged
    - no generic JSON or partial-success snapshot boundary
    - no CLI, API, persistence, or automatic runtime collection
  lane_map:
    core_model: core snapshot model worker
    canonical_identity: canonical identity worker
    repository_adapter: repository adapter worker
    context_adapter: context adapter worker
    fingerprint_adapter: fingerprint adapter worker
    adversarial_verification: adversarial verification worker
    integration: integration and release verifier
```
