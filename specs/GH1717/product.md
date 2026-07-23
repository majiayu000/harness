# Product Spec

## Linked Issue

GH-1717

## User Problem

Clients use the typed `context/preview` JSON-RPC contract to inspect a context
composition before starting agent work. Today that one protocol method exposes
types owned by the context-composer implementation crate. As a result,
consumers that only need protocol messages also acquire the composer and its
rules, skills, GC, and exec dependencies. The transport contract and the
implementation graph cannot evolve independently.

## Goals

- Make the protocol layer own the typed `context/preview` request contract.
- Remove the normal `harness-protocol -> harness-context` dependency.
- Preserve the existing JSON field names, nesting, enum values, defaults, and
  successful response behavior.
- Keep malformed requests fail-closed and make the server boundary conversion
  explicit, exhaustive, and testable.

## Non-Goals

- Changing context composition, provider selection, budgeting, degradation,
  manifests, endpoint names, or response payloads.
- Adding protocol-owned response DTOs. `RpcResponse.result` remains
  `serde_json::Value`; protocol-only Rust consumers continue to deserialize the
  stable response JSON into their own type or inspect it as JSON.
- Reusing `harness_core::types::ContextItem`, which models agent execution
  context and has a different shape and meaning.
- Replacing typed fields with `serde_json::Value`, `Any`, aliases, or silent
  fallback behavior.
- Introducing a shared-types crate or making `harness-context` depend on
  transport-layer types in this change.
- Guaranteeing source compatibility for Rust callers that directly construct
  `Method::ContextPreview` with `harness_context` types; this risk must be
  documented and reviewed before implementation merge.

## User-Visible Behavior

1. **B-001:** Building or consuming `harness-protocol` no longer requires a
   normal dependency on `harness-context`, and the context implementation
   crates are not reachable through the protocol dependency graph.
2. **B-002:** Every JSON request that is valid under the current
   `context/preview` contract remains valid with identical field names,
   nesting, snake_case enum values, and serialized representation.
3. **B-003:** Missing optional `run_id` and `dedupe_key` remain absent.
   Omitted `degrade`, `supplied_items`, and `target_paths` remain equivalent
   to explicit empty lists, and absent optional task-profile fields retain
   their current default and serialization behavior.
4. **B-004:** Missing required fields, unknown enum values, and malformed
   degradation payloads remain explicit request-deserialization failures. The
   boundary must not invent values or continue with a partial request.
5. **B-005:** For a valid request, conversion preserves every request and item
   value: IDs, paths, task-profile fields, budget, item order, content, token
   estimate, class, priority, relevance, all degradation variants and NAP
   status, dedupe key, and instruction-bearing flag.
6. **B-006:** Repeated conversion of the same valid request is deterministic,
   does not mutate caller-visible data, and does not reorder supplied items or
   degradation ladders.
7. **B-007:** After conversion, the server invokes the existing context
   composer preview path, so rendered content, manifest decisions, run-ID
   fallback, error propagation, and response shape remain unchanged.
8. **B-008:** Completion evidence includes deterministic protocol and server
   tests plus dependency-graph evidence; passing behavior tests without proving
   removal of the dependency edge is incomplete.

## Acceptance Criteria

- [ ] B-001 through B-008 have deterministic implementation and verification
      evidence.
- [ ] Golden protocol fixtures captured from the current composer-owned types
      prove the full valid wire shape, defaults, `None` serialization, every
      degradation variant, and representative malformed payloads.
- [ ] Server tests prove exhaustive field preservation and unchanged
      `ContextComposer::compose_supplied` behavior.
- [ ] Dependency evidence proves `harness-context`, `harness-rules`,
      `harness-skills`, `harness-gc`, and `harness-exec` are not reachable
      through `harness-protocol`.
- [ ] The implementation PR documents the Rust source-compatibility impact for
      direct `Method::ContextPreview` constructors.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-003 and B-004. |
| Error and failure paths | Covered by B-004 and B-007. |
| Authorization / permission | N/A: request decoding and conversion make no authorization decision. Existing endpoint authorization remains unchanged. |
| Concurrency / race / ordering | Covered by B-005 and B-006; conversion is local and must preserve order. |
| Retry / repetition / idempotency | Covered by B-006. |
| Illegal state transitions | N/A: this change has no lifecycle state or persistence transition. |
| Compatibility / migration | Covered by B-002, B-003, B-005, and B-007. |
| Degradation / fallback | Covered by B-004 and B-005; degradation data is preserved and malformed data cannot silently fall back. |
| Evidence and audit integrity | Covered by B-008. |
| Cancellation / interruption / partial completion | N/A: decoding and conversion are synchronous and stateless; no partial state is committed. |

## Edge Cases

- `supplied_items`, `degrade`, and `target_paths` omitted versus explicitly
  empty; each pair must deserialize to the same typed value.
- A task profile with every optional field absent and `target_paths` omitted.
- `run_id` and `dedupe_key` omitted independently.
- Empty content, zero `est_tokens`, zero `budget_hint`, and boundary relevance
  values accepted by the current contract.
- Mixed `summary`, `pointer`, and `summarized` degradation entries, including
  every valid NAP status, in a caller-defined order.
- Unknown item class, priority, degradation level, or malformed nested
  `summarized` content.
- A current run identity exists while the request omits `run_id`; server
  fallback remains unchanged.

## Rollout Notes

The JSON-RPC contract needs no client migration. Rust callers that construct
`Method::ContextPreview` directly will use protocol-owned DTOs after this
change, so the implementation PR and release notes must identify that source
compatibility change. If repository evidence shows that preserving Rust type
identity is mandatory, stop implementation and return to spec review for the
lightweight shared-types alternative rather than reintroducing the dependency
or adding an untyped bridge.
