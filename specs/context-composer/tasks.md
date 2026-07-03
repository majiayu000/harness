# Context Composer Task Plan

Spec: `specs/context-composer/` (PR #1426)

## Thread Lane Map

- `CC-L1`: implementation worker. Owns the new `harness-context` crate, provider adapters, server wiring, and their tests.
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

- Manifest schema (v1) rendered per composition and logged through harness-observe as event kind `context_manifest`.
- Manifests record item ids, sizes, and decisions — never full item content.

Verify:

```sh
cargo test -p harness-context manifest
```

### CC-T4: Five v1 providers

Owner: CC-L1

Dependencies: CC-T1

Done when:

- Providers wrapping existing crates: rules (harness-rules), skills (harness-skills), contract (harness-exec ExecPlan), task brief, GC drafts (harness-gc).
- Providers reuse existing selection logic; they add only sizing, relevance, degrade ladders, and dedupe keys.

Verify:

```sh
cargo test -p harness-context providers
```

### CC-T5: Shadow-mode server wiring

Owner: CC-L1

Dependencies: CC-T2, CC-T3, CC-T4

Done when:

- `harness-server` invokes the composer at `thread/start` and `thread/resume` when `context.mode = "shadow"` (default); `turn/steer` never recomposes.
- Injection remains byte-identical to the pre-composer baseline (integration test against a captured baseline).
- Composer failure in shadow mode produces an error event only and never affects thread start.
- Config surface: `budget_tokens` (per agent kind), `reserved_headroom` (default 0.20), `provider_timeout_ms`, `quotas` (defaults: rule 0.30 / skill 0.25 / contract 0.25 / brief 0.15 / draft 0.05).

Verify:

```sh
cargo test -p harness-server context_shadow
```

### CC-T6: RPC methods

Owner: CC-L1

Dependencies: CC-T5

Done when:

- `context/preview` dry-runs a composition for a hypothetical request; `context/manifest/get` returns a thread's manifest.
- Both registered in the JSON-RPC router with protocol tests.

Verify:

```sh
cargo test -p harness-server context_rpc
```

### CC-T7: Enforce mode (gated)

Owner: CC-L1

Dependencies: CC-T5 plus a shadow-mode review period on real tasks

Done when:

- Per-project `context.mode = "enforce"` disables the five legacy injection paths behind the same flag and makes the composed block the single harness-side injection.
- Integration test asserts the legacy paths inject nothing under enforce.
- Rollback documented: flip the flag back to `shadow`; legacy paths are disabled, not deleted, until enforce has run stably.

Verify:

```sh
cargo test -p harness-server context_enforce
```
