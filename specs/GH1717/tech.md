# Tech Spec

## Linked Issue

GH-1717

## Product Spec

See `specs/GH1717/product.md`.

<!-- specrail-planned-changes
{"issue":1717,"complete":true,"paths":["Cargo.lock","crates/harness-protocol/Cargo.toml","crates/harness-protocol/src/context.rs","crates/harness-protocol/src/lib.rs","crates/harness-protocol/src/methods.rs","crates/harness-server/src/handlers/context.rs","crates/harness-server/src/router/tests/observability.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008"]}
-->

## Current System

- `crates/harness-protocol/Cargo.toml:12-21` declares protocol dependencies;
  line 15 adds the direct `harness-context` edge.
- `crates/harness-protocol/src/methods.rs:152-157` uses
  `harness_context::ComposeRequest` and `harness_context::ContextItem` in the
  public `Method::ContextPreview` variant. These are the only
  `harness-context` uses in protocol source.
- `crates/harness-context/src/types.rs:48-70` defines `ComposeRequest` and its
  nested `TaskProfile`; lines 72-183 define the item enums, degradation
  contract, and `ContextItem`.
- `crates/harness-context/Cargo.toml:12-17` depends on `harness-core`,
  `harness-rules`, `harness-skills`, `harness-gc`, and `harness-exec`, so the
  protocol edge reaches the full implementation graph.
- `crates/harness-protocol/src/methods.rs:335-378` contains the current
  deserialization test but covers only one pointer degradation and does not
  prove default, malformed, or round-trip compatibility.
- `crates/harness-server/src/router/mod.rs:177-181` dispatches the typed method
  fields directly to the context handler.
- `crates/harness-server/src/handlers/context.rs:12-23` currently accepts
  composer-domain types, fills a missing run ID, and calls
  `ContextComposer::compose_supplied`.
- `crates/harness-server/src/router/tests/observability.rs:610-666` verifies
  that a supplied rule reaches the preview manifest.
- `crates/harness-core/src/types.rs:596-606` defines a different
  `ContextItem` enum for agent execution; its tagged variants cannot represent
  the composer preview item contract.

## Proposed Design

Add `crates/harness-protocol/src/context.rs` containing protocol-owned,
serde-derived DTOs dedicated to `context/preview`:

- `ContextPreviewRequest` with `thread_id`, optional `run_id`, `project`,
  `task_profile`, and `budget_hint`.
- `ContextPreviewTaskProfile` with the current optional strings and defaulted
  `target_paths`.
- `ContextPreviewItemId(pub String)`, a protocol-local
  `#[serde(transparent)]` newtype whose JSON representation is the same string
  representation as the current composer `ItemId`.
- `ContextPreviewItem` with the current item fields and defaults, using
  `ContextPreviewItemId` for `id`.
- Closed wire enums for item class, priority, and degradation. The degradation
  enum retains the current internally tagged `level`, `content`, snake_case
  representation, including structured `summarized` content and `NapStatus`.

Use existing `harness-core` thread, project, run, and NAP types where they are
already protocol dependencies and exactly match the wire contract. Convert
`ContextPreviewItemId` explicitly to `harness_context::ItemId` in the server
handler. Do not move the composer `ItemId` or use the unrelated core agent
`ContextItem`.

Change `Method::ContextPreview` to hold the protocol DTOs. Keep the field names
`request` and `supplied_items` and the existing serde default on
`supplied_items`. Export the DTO module from `harness-protocol::lib`.

At the start of `handlers::context::context_preview`, convert the request and
each supplied item into the corresponding `harness-context` domain type using
private, typed conversion functions. Every closed enum is matched
exhaustively; every struct field is named explicitly. The conversion is
infallible after serde validation and preserves vector order. Only after
conversion does the existing run-ID fallback and
`ContextComposer::compose_supplied` call execute.

Remove `harness-context` from `harness-protocol/Cargo.toml` and refresh the
`harness-protocol` dependency list in `Cargo.lock`. No context-composer source,
router dispatch, public endpoint documentation, response schema, or
persistence changes are required.

## Data Flow

`JSON-RPC bytes -> RpcRequest/Method deserialization into protocol DTOs ->
exhaustive server conversion -> existing ComposeRequest/ContextItem domain
values -> existing run-ID fallback -> ContextComposer::compose_supplied ->
existing RpcResponse`.

Malformed bytes stop during typed deserialization. Conversion performs no I/O,
fallback, filtering, sorting, or persistence.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | protocol manifest and lockfile | `cargo check -p harness-protocol --all-targets`; `! cargo tree -p harness-protocol --edges normal \| rg -q 'harness-(context\|rules\|skills\|gc\|exec) '` |
| B-002 | protocol DTO serde definitions, item-ID newtype, and `Method` | `cargo test -p harness-protocol context_preview_wire_json_matches_legacy_golden_fixtures --lib` compares literal JSON captured from the current composer-owned types with the new DTO output |
| B-003 | DTO serde defaults | `cargo test -p harness-protocol context_preview_defaults_and_empty_collections_are_equivalent --lib` proves omitted and explicit-empty vectors deserialize equally and asserts current `None` serialization |
| B-004 | closed enums and required DTO fields | `cargo test -p harness-protocol context_preview_rejects_malformed_payloads --lib` |
| B-005 | server conversion functions | `cargo test -p harness-server context_preview_conversion_preserves_every_field --lib` |
| B-006 | server conversion functions | `cargo test -p harness-server context_preview_conversion_is_deterministic_and_order_preserving --lib` |
| B-007 | existing handler/composer path and router integration | `cargo test -p harness-server context_rpc_preview_with_supplied_items_returns_manifest --lib` |
| B-008 | protocol/server tests and dependency evidence | Run all commands above and `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1717` |

## Alternatives Considered

- Add a lightweight `harness-context-types` crate and re-export its types from
  `harness-context`: this avoids duplicate DTO/domain structures and better
  preserves Rust type identity, but adds a workspace crate and moves multiple
  nested domain types. It is the fallback if public Rust source compatibility
  is a required gate.
- Move composer-domain types into `harness-protocol` and make
  `harness-context` depend on protocol: rejected because domain behavior would
  depend on the transport layer and crate-private type helpers would cross the
  ownership boundary.
- Reuse `harness_core::types::ContextItem`: rejected because it is a
  semantically different tagged enum without composer metadata.
- Deserialize through `serde_json::Value` or a generic map: rejected because
  it weakens validation, permits drift, and creates silent partial-conversion
  paths.
- Keep the current dependency: rejected because one wire method continues to
  impose the implementation graph on every protocol consumer.

## Risks

- Security: no authorization or external-call change is planned. Typed closed
  enums retain fail-closed decoding; review must reject untyped fallback.
- Compatibility: the JSON contract remains stable, but direct Rust
  construction of `Method::ContextPreview` changes from `harness-context`
  types to protocol DTOs. The implementation PR must call this out. If source
  compatibility is mandatory, return to spec review for the shared-types
  alternative.
- Data integrity: duplicated wire/domain fields can drift. Exhaustive
  conversion and legacy golden JSON fixtures must change whenever either side
  adds a field or enum variant.
- Performance: conversion clones or moves bounded request data once before
  composition. Tests should use owned conversion so no avoidable second clone
  is introduced.
- Maintenance: protocol and domain models now have separate owners by design;
  explicit mapping is the enforcement point, not an alias or wildcard.

## Test Plan

- [ ] Run every command in the Product-to-Test Mapping.
- [ ] Compare new DTO serialization against literal golden JSON captured from
      the current composer-owned types, including omitted versus explicit-empty
      inputs, `None` serialization, every item class and priority, `summary`,
      `pointer`, structured `summarized`, and every valid `NapStatus`
      representation including `failed { fell_back }`.
- [ ] Prove omitted and explicit-empty `supplied_items`, `degrade`, and
      `target_paths` deserialize to equal typed values.
- [ ] Assert malformed required fields, enum values, and nested summarized
      content fail deserialization before handler execution.
- [ ] Use a full-field fixture to assert conversion equality field by field
      and preserve supplied-item and degradation-ladder ordering.
- [ ] Run `cargo check -p harness-protocol --all-targets`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Before pushing the implementation PR, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1717`.
- [ ] Collect exact-head CI, Gemini review, independent reviewer evidence,
      review-thread state, and SpecRail PR-gate evidence.

## Rollback Plan

Revert the implementation PR, restoring the two composer-domain types in
`Method::ContextPreview` and the direct protocol dependency. No schema,
persistence, or data migration rollback is required. If a compatibility issue
is discovered before merge, keep the implementation unmerged and revise the
approved specs for the lightweight shared-types design instead.
