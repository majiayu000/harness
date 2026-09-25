# Context Composer Task Plan

Spec: `specs/context-composer/` (PR #1426)

## Thread Lane Map

- `CC-L1`: implementation worker. Owns the `harness-context` crate, provider adapters, preview RPC wiring, and their tests.
- `CC-L2`: reviewer, read-only. Owns post-implementation diff review against this spec, with special attention to determinism and no-silent-drop guarantees.

No two writable lanes may edit the same file. `AGENTS.md`, `.claude/*`, hooks, settings, and global config are forbidden files unless explicitly requested.

## Tasks

### CC-T1: harness-context crate skeleton

Owner: CC-L1

Dependencies: spec merged

Done when:

- New workspace crate `harness-context` with `ContextItem`, `ComposeRequest`, `ContextProvider`, `Priority`, `Degraded`, `ProviderError` types.
- `Estimator` trait with the `bytes / 4` v1 implementation and its error margin documented.

Verify:

```sh
cargo test -p harness-context types
```

### CC-T2: Arbitration pipeline

Owner: CC-L1

Dependencies: CC-T1

Done when:

- Deterministic pipeline: collect (per-provider timeout) → dedupe (priority, provider precedence, item-id tiebreak) → mandatory P0 (overflow = hard `compose_error`) → quota fill with degrade ladders → global redistribution → constraint-count guard (> 15 instruction-bearing items degrades lowest-score P2 to pointers and records a warning).
- Golden tests: fixture proposals → byte-exact snapshot of composition + manifest.
- Property tests: budget never exceeded; every input item appears in the manifest exactly once with a decision.

Verify:

```sh
cargo test -p harness-context pipeline
```

### CC-T3: Manifest emission

Owner: CC-L1

Dependencies: CC-T2

Done when:

- Manifest schema (v1) is rendered per composition and returned in the `context/preview` response. It is response-only and is not persisted through harness-observe.
- Manifests record item ids, sizes, and decisions — never full item content.

Verify:

```sh
cargo test -p harness-context manifest
```

### CC-T4: Preview providers

Owner: CC-L1

Dependencies: CC-T1

Done when:

- Providers wrapping existing sources: rules (harness-rules), skills (harness-skills), task contract, ExecPlan contracts (harness-exec), task brief, and GC drafts supplied by the server.
- Providers reuse existing selection logic; they add only sizing, relevance, degrade ladders, and dedupe keys.
- Contract and ExecPlan providers use distinct provider IDs.

Verify:

```sh
cargo test -p harness-context providers
```

### CC-T5: Preview-only server wiring

Owner: CC-L1

Dependencies: CC-T2, CC-T3, CC-T4

Done when:

- `harness-server` invokes the composer only for `context/preview`; `thread/start`, `thread/resume`, and `turn/steer` never compose.
- Production agent requests remain on existing prompt assembly paths.
- Config surface: `budget_tokens` (per agent kind), `reserved_headroom` (default 0.20), `provider_timeout_ms`, `quotas` (defaults: rule 0.30 / skill 0.25 / contract 0.25 / brief 0.15 / draft 0.05).
- Legacy `context.mode` config is ignored for compatibility and is no longer part of typed config.

Verify:

```sh
cargo test -p harness-server context_preview
```

### CC-T6: RPC methods

Owner: CC-L1

Dependencies: CC-T5

Done when:

- `context/preview` dry-runs a composition for a hypothetical request and returns rendered context plus its manifest.
- Both registered in the JSON-RPC router with protocol tests.

Verify:

```sh
cargo test -p harness-server context_rpc
```

### CC-T7: Enforce mode (gated)

Status: superseded by GH1806 preview-only decision.

Future execution integration must be planned in a new task or spec that owns prompt assembly, agent request wiring, migration behavior, and rollback.
