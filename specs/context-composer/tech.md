# Context Composer Tech Spec

## Context

Harness injects context into agent threads from at least five production code paths: harness-rules (loaded rule text), harness-skills (skill projection), harness-exec (ExecPlan/spec contract), the task brief itself, and harness-gc (remediation drafts). Each path decides independently what to include. GH1806 records the current decision: `harness-context` remains a preview/debug composer invoked by `context/preview`; it does not run at `thread/start` or `thread/resume`, and it does not write to `AgentRequest.context`.

CLI-side hook injections (remem, vibeguard, user hooks) are outside the composer's control by design; they are accounted for with reserved headroom (see Budget).

## Design

### Provider trait

```rust
pub trait ContextProvider: Send + Sync {
    fn id(&self) -> ProviderId;                       // "rules", "skills", "contract", "exec-plan", "brief", "gc-drafts"
    fn propose(&self, req: &ComposeRequest) -> Result<Vec<ContextItem>, ProviderError>;
}

pub struct ComposeRequest {
    pub thread_id: ThreadId,
    pub run_id: Option<RunId>,          // from the session-identity spec, when present
    pub project: ProjectId,
    pub task_profile: TaskProfile,      // task kind, target paths, agent kind (claude|codex)
    pub budget_hint: Tokens,
}

pub struct ContextItem {
    pub id: ItemId,
    pub class: ItemClass,               // Rule | Skill | Contract | Brief | Draft
    pub content: String,
    pub est_tokens: Tokens,             // provider's estimate; composer re-estimates
    pub priority: Priority,             // P0 (mandatory) | P1 | P2
    pub relevance: f32,                 // 0.0..=1.0, provider-scored for this task
    pub degrade: Vec<Degraded>,         // ordered ladder: e.g. [Summary(String), Pointer(String)]
    pub dedupe_key: Option<String>,     // same key across providers → single survivor
    pub instruction_bearing: bool,      // counts toward the constraint-count guard
}
```

Providers adapt existing crates; they do not reimplement selection logic they already have — e.g. the skills provider wraps harness-skills' existing discovery/dedup and only adds sizing, relevance, and a degrade ladder.

### Arbitration pipeline (deterministic, no model calls)

```
collect → dedupe → mandatory → quota fill → redistribute → guards → render
```

1. **Collect.** Run providers with a per-provider timeout (config, default 2s). A provider error or timeout contributes nothing, is logged at error level, and is recorded in the manifest as `provider_error` — the composition is marked degraded, never silently complete.
2. **Dedupe.** Items sharing a `dedupe_key` collapse to one survivor: highest priority wins, ties broken by configured provider precedence (`rules > contract > exec-plan > skills > brief > gc-drafts`), then lexicographic item id (for determinism).
3. **Mandatory.** All P0 items are placed first. If P0 alone exceeds the budget, composition **fails loudly** (`compose_error`) rather than silently trimming mandatory context.
4. **Quota fill.** Remaining budget splits into class quotas (default: Rule 30%, Skill 25%, Contract 25%, Brief 15%, Draft 5%). Within each class, items sort by `score = relevance × class_weight`, ties by item id. Each item is placed at the highest degrade level that fits its class's remaining quota: full content, else ladder steps in order, else deferred to step 5.
5. **Redistribute.** Unused quota pools globally; deferred items compete in global score order, same degrade-to-fit rule.
6. **Guards.**
   - Items still unplaced → `excluded` with reason `budget`.
   - Count items with `instruction_bearing = true`; if > 15, degrade the lowest-scoring instruction-bearing P2 items to `Pointer` until ≤ 15 or none remain to degrade, and record a `constraint_overload` warning either way.
7. **Render.** Emit the composed block (stable section order: contract, rules, skills, brief, drafts) plus the manifest.

### Budget model

```toml
[context]
budget_tokens = 24000            # per agent kind overridable: [context.claude-code] / [context.codex]
reserved_headroom = 0.20         # fraction left free for CLI-hook injections the composer cannot see
provider_timeout_ms = 2000
quotas = { rule = 0.30, skill = 0.25, contract = 0.25, brief = 0.15, draft = 0.05 }
```

Effective budget = `budget_tokens × (1 − reserved_headroom)`. Token estimation is a pluggable `Estimator`; v1 ships `bytes / 4` with its error margin documented. The interface admits a real tokenizer later without touching the pipeline.

### Manifest

Returned by `context/preview`:

```json
{"v":1,"thread_id":"...","run_id":"ar-01j1...","mode":"preview",
 "budget":{"total":24000,"effective":19200,"used":14730},
 "items":[
   {"id":"rule:U-29","class":"rule","decision":"included","tokens":410},
   {"id":"skill:tdd-guide","class":"skill","decision":"degraded","level":"pointer","tokens":28,"reason":"quota"},
   {"id":"draft:gc-114","class":"draft","decision":"excluded","reason":"budget"}
 ],
 "provider_errors":[],"warnings":["constraint_overload:17"]}
```

`context/preview` runs steps 1-7 for a hypothetical `ComposeRequest` without starting a thread. Manifests are returned to the caller and are not persisted by the composer.

### Modes

There is one active mode: `preview`. The former shadow/enforce rollout surface was removed because no production call site constructed those modes or consumed composed output. Future execution integration must introduce its own spec, tests, and migration path before composed context can affect agent requests.

### Interaction with unmanaged injectors

remem, vibeguard, and user hooks inject inside the CLI where harness cannot (and should not) intercept. v1 accounts for them only via `reserved_headroom`. Follow-up (requires session-identity): join the run's JSONL to its manifest by run id, measure actual hook-injected volume, and report headroom accuracy per project — turning the 20% guess into a measured value.

### Outcome feedback (follow-up, design constraint only)

harness-observe already grades task quality. Manifests give every graded task a record of its context decisions, so quota/weight tuning becomes an offline analysis producing **adoptable drafts** (the existing GC draft-adopt pattern). Auto-application is explicitly out; the pipeline stays deterministic and human-governed.

## Data Model

No new store. Preview manifests are returned in the RPC response; config lives in the existing TOML. The crate is stateless between compositions.

## Privacy and Failure Behavior

- Manifests record item ids, sizes, and decisions — not full item content (content is recoverable from providers by id).
- Provider failure: error-level log + degraded manifest, never a silent partial composition (U-29).
- Composer errors affect only the preview RPC response. Production thread start remains on the existing prompt assembly paths.

## Verification Plan

- Golden tests: fixture proposals → snapshot of composition + manifest (byte-exact, guards determinism).
- Property tests: `used ≤ effective budget` for arbitrary item sets; every input item appears in the manifest exactly once with a decision.
- Pipeline unit tests: dedupe precedence, P0-overflow hard failure, degrade-ladder fitting, redistribution order, constraint-count guard.
- Preview RPC integration: assert rendered context and manifest are returned for a hypothetical request.
- Config regression: legacy `context.mode` TOML is ignored for compatibility, and the typed config no longer exposes a mode field.

## Rollback

Preview-only composition is inert for production execution. Roll back by disabling or removing the preview RPC route; there is no production injection flag to flip.
