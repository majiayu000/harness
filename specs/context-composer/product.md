# Context Composer Product Spec

## Summary

The context composer is a preview/debug tool for inspecting the context Harness could assemble from registered providers. Today rules, skills, ExecPlan contracts, task briefs, and GC drafts are still injected by independent production paths; wiring the composer into agent execution requires a separate integration design. The composer collects typed proposals from these sources, arbitrates them inside an explicit budget, and returns an auditable manifest from `context/preview`.

## User Problem

Two failure modes, both invisible today:

1. **Constraint overload.** Every injector adds "just one more" block. Research and our own VibeGuard U-32 experience agree that past ~15 active constraints agent compliance degrades — but nothing counts, so nobody notices until output quality drops.
2. **No audit trail.** When a task goes wrong, there is no record of which rules, skills, and contract text were actually in the context at thread start. Debugging "why did the agent ignore rule X" is guesswork.

## Product Behavior

- For `context/preview`, registered providers propose typed context items with a declared size, priority, and degrade ladder (full text -> summary -> one-line pointer).
- The composer deduplicates, scores, and fits items into a configured token budget. Items that don't fit are degraded down their ladder before being excluded; every exclusion has a recorded reason.
- Every preview composition returns a **manifest**: what was included, degraded, excluded, and why, with size accounting.
- The composer does not run at `thread/start`, `thread/resume`, or `turn/steer`, and it does not write to `AgentRequest.context`.
- There is no active shadow or enforce mode in the composer surface. Existing Harness injection paths remain authoritative for execution.

## MVP Scope

- `harness-context` crate with the provider trait and deterministic arbitration pipeline.
- Preview providers, all sources Harness already owns: rules (harness-rules), skills (harness-skills), task contract, ExecPlan contracts (harness-exec), task brief, GC remediation drafts.
- `context/preview` RPC method returning rendered context plus the manifest.
- Config: per-project budget, per-class quotas, reserved headroom, and provider timeout. No mode flag.

## Follow-Up Scope

- Execution integration after a dedicated design decides how composed output replaces or augments existing prompt assembly.
- Manifest persistence, if needed, after preview manifests prove useful enough to query outside the immediate RPC response.
- Measure **unmanaged injectors** (CLI-side hooks: remem, vibeguard, user hooks) by joining session JSONL to manifests via the run id (depends on the session-identity spec), so the reserved headroom becomes measured instead of guessed.
- Outcome-driven tuning: correlate manifests with harness-observe quality grades; propose weight adjustments as adoptable drafts (same pattern as GC), never auto-applied.
- Memory provider — explicitly deferred until the remem × harness integration question is decided; the design must not assume it.

## Non-Goals

- Not a RAG/retrieval system and not a prompt rewriter; v1 makes no model calls and is fully deterministic.
- Does not touch CLI-side hook injections (remem, vibeguard hooks keep working exactly as today); it only governs what harness itself injects.
- Does not manage the agent's in-conversation context window (compaction etc.) — only the initial/turn-start injection.

## Acceptance Criteria

- `context/preview` returns rendered context and a manifest without changing production agent requests.
- Determinism: identical inputs produce byte-identical composition and manifest.
- The budget is never exceeded; mandatory items that cannot fit fail the composition loudly rather than being silently trimmed.
- Every excluded or degraded item appears in the manifest with a reason. No silent drops.
- A count of instruction-bearing items > 15 raises a recorded warning in the manifest.
- Contract and ExecPlan providers use distinct provider IDs, so manifest attribution is unambiguous.
- `harness-context` does not depend on crates it does not use.

## Decisions

- GH1806 records the active product decision: keep `harness-context` preview-only. Do not expose shadow/enforce modes or route composed output into `AgentRequest.context` until a future execution-integration spec owns that blast radius.
- Class quotas ship with the proposed defaults (rule 30 / skill 25 / contract 25 / brief 15 / draft 5). They are configuration, and preview manifests are the instrument for revising them.
- v1 token estimation is `bytes / 4` behind the `Estimator` trait: deterministic and dependency-free. A real tokenizer is a drop-in replacement later.
- Recomposition is preview-only. `turn/steer` never recomposes mid-turn; steering text is user intent, not managed context.
