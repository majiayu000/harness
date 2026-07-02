# Context Composer Product Spec

## Summary

A single owner for the context harness injects when it starts an agent thread. Today rules, skills, ExecPlan contracts, task briefs, and GC drafts are injected by independent code paths with no shared token budget and no record of what the agent actually received. The Context Composer collects typed proposals from these sources, arbitrates them inside an explicit budget, and emits an auditable manifest for every composition. It ships in shadow mode first: observe and log what it *would* do before it changes anything.

## User Problem

Two failure modes, both invisible today:

1. **Constraint overload.** Every injector adds "just one more" block. Research and our own VibeGuard U-32 experience agree that past ~15 active constraints agent compliance degrades — but nothing counts, so nobody notices until output quality drops.
2. **No audit trail.** When a task goes wrong, there is no record of which rules, skills, and contract text were actually in the context at thread start. Debugging "why did the agent ignore rule X" is guesswork.

## Product Behavior

- At `thread/start` (and on resume), registered providers propose typed context items with a declared size, priority, and degrade ladder (full text → summary → one-line pointer).
- The composer deduplicates, scores, and fits items into a configured token budget. Items that don't fit are degraded down their ladder before being excluded; every exclusion has a recorded reason.
- Every composition emits a **manifest**: what was included, degraded, excluded, and why, with size accounting — stored as a harness-observe event, queryable later.
- `context/preview` lets a human dry-run the composition for a task before running it.
- **Shadow mode (default at launch):** the composer runs and logs manifests, but injection continues through the existing code paths, byte-identical to today. **Enforce mode** switches injection over to the composed output.

## MVP Scope

- New `harness-context` crate with the provider trait and deterministic arbitration pipeline.
- Five v1 providers, all sources harness already owns: rules (harness-rules), skills (harness-skills), ExecPlan/spec contract (harness-exec), task brief, GC remediation drafts (harness-gc).
- Shadow mode + manifest logging + `context/preview` and `context/manifest/get` RPC methods.
- Config: per-project budget, per-class quotas, mode flag.

## Follow-Up Scope

- Enforce mode rollout after shadow-mode manifests are reviewed against real tasks.
- Measure **unmanaged injectors** (CLI-side hooks: remem, vibeguard, user hooks) by joining session JSONL to manifests via the run id (depends on the session-identity spec), so the reserved headroom becomes measured instead of guessed.
- Outcome-driven tuning: correlate manifests with harness-observe quality grades; propose weight adjustments as adoptable drafts (same pattern as GC), never auto-applied.
- Memory provider — explicitly deferred until the remem × harness integration question is decided; the design must not assume it.

## Non-Goals

- Not a RAG/retrieval system and not a prompt rewriter; v1 makes no model calls and is fully deterministic.
- Does not touch CLI-side hook injections (remem, vibeguard hooks keep working exactly as today); it only governs what harness itself injects.
- Does not manage the agent's in-conversation context window (compaction etc.) — only the initial/turn-start injection.

## Acceptance Criteria

- Shadow mode produces a manifest for 100% of thread starts; injection remains byte-identical to the pre-composer baseline (verified by integration test).
- Determinism: identical inputs produce byte-identical composition and manifest.
- The budget is never exceeded; mandatory items that cannot fit fail the composition loudly rather than being silently trimmed.
- Every excluded or degraded item appears in the manifest with a reason. No silent drops.
- A count of instruction-bearing items > 15 raises a recorded warning in the manifest.

## Decisions

- Class quotas ship with the proposed defaults (rule 30 / skill 25 / contract 25 / brief 15 / draft 5). They are configuration, and shadow-mode manifests are the instrument for revising them — no further quota debate before that data exists.
- v1 token estimation is `bytes / 4` behind the `Estimator` trait: deterministic, dependency-free, and shadow mode makes its error harmless. A real tokenizer is a drop-in replacement later.
- Recomposition happens at `thread/start` and `thread/resume` only. `turn/steer` never recomposes mid-turn — steering text is user intent, not managed context.
