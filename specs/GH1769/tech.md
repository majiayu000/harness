# Tech Spec

## Linked Issue

GH-1769

## Current State (verified at commit `81c78255`)

### Skill matching

- `crates/harness-context/src/providers.rs:71-93` — `SkillsProvider`
  lowercases the prompt and selects a skill when `trigger_patterns` is empty
  or any pattern is a literal substring
  (`.any(|pattern| prompt.contains(&pattern.to_lowercase()))`, line 80).
  Relevance is a constant chosen only by whether trigger patterns exist
  (line 93).
- `crates/harness-core/src/prompts/context.rs:82` —
  `build_matched_skills_section` renders matched skills.
- **Only consumer outside the defining crates:**
  `crates/harness-server/src/handlers/context.rs:115` (`context_preview`
  RPC). The live runtime packet builder
  (`crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs`) has
  no skills section; grep for `SkillsProvider` /
  `build_matched_skills_section` / `trigger_patterns` under
  `harness-server/src` matches only `handlers/context.rs`. Skill governance
  (`crates/harness-skills/src/store.rs:14` `SkillGovernanceStatus`,
  `store.rs:43` `canary_ratio`) therefore governs an injection path that does
  not exist at runtime.

### Repo memory retrieval

- `crates/harness-workflow/src/runtime/memory_retrieval.rs:7-9` — limits:
  top-5 injected, 800-token budget, 50-row candidate fetch.
- `memory_retrieval.rs:44-56` — SQL: `WHERE repo = $1` then
  `ORDER BY CASE WHEN activity_class = $2 THEN 0 ELSE 1 END ASC,
  created_at DESC LIMIT $3`.
- `memory_retrieval.rs:85-97` — `rank_repo_memory_candidates` re-sorts by the
  same (activity-class match, recency) keys in process.
- Injection: `workflow_runtime_worker/prompt_packet.rs:88-89` sets
  `packet["repo_memory"]` (`harness.runtime.repo_memory.v1`,
  `prompt_packet.rs:259-263`), rendered with an untrusted preamble via
  `repo_memory_prompt.rs`.

### Context Composer

- `crates/harness-context/src/composer.rs` (652 lines): class budgets,
  dedupe, degradation ladder, selection manifest.
- Only external call site: `handlers/context.rs:17` (`context_preview`).
  Zero execution-path call sites; `specs/context-composer/product.md` staged
  shadow-first and enforce never landed.
- Adjacent landed work: `specs/GH1732/` (runtime context provenance) defines
  the provenance manifest for prompt-packet sources; the Composer manifest is
  the natural producer for it.

### Complexity routing

- `crates/harness-server/src/complexity_router.rs:41` — `classify` counts
  file-path-shaped tokens (`complexity_router.rs:36`) and maps count →
  complexity tier. Out of scope for change here (Non-Goal), documented for
  completeness.

## Design

### 1. Retrieval trait and implementations

New module `crates/harness-context/src/retrieval.rs`:

```rust
pub struct RetrievalQuery<'a> {
    pub surface: RetrievalSurface,          // Skill | RepoMemory
    pub text: &'a str,                      // prompt or activity brief
    pub activity_class: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub limit: usize,
}

pub struct ScoredCandidate {
    pub id: String,
    pub score: f64,
    pub native_repo: bool,
}

pub trait KnowledgeRetriever: Send + Sync {
    fn name(&self) -> &'static str;
    fn rank(&self, query: &RetrievalQuery<'_>, candidates: &[Candidate])
        -> Result<Vec<ScoredCandidate>, RetrievalError>;
}
```

Implementations:

- `SubstringRetriever` — extracted verbatim from `providers.rs:76-93` and
  `memory_retrieval.rs:85-97`; characterization tests pin byte-identical
  selection.
- `EmbeddingRetriever` — cosine over stored embeddings. Storage: new nullable
  column `embedding vector(D)` on `workflow_repo_memory` and on the skill
  store's persistence (pgvector when available; when the extension is absent,
  an in-process index built from stored `f32` blobs — no external service).
  Embeddings are computed at record write time; the query embedding is
  computed once per retrieval. Records with NULL embedding fall back to the
  substring score for that candidate, flagged in telemetry (product edge
  case).

Candidate fetch stays SQL-prefiltered (existing 50-row query for memory; the
skill store's discovery list for skills), so ranking cost is bounded.

### 2. Shadow comparison

`RetrievalExecutor` wraps a primary and optional shadow retriever:

- Runs primary; if shadow configured, runs shadow on the same inputs.
- Emits one `retrieval_comparison` event to the observe event stream
  (`harness-observe` EventStore) per retrieval: surface, implementation
  names, both ranked id/score lists, `overlap_at_k`, Kendall-tau-style rank
  divergence, shadow latency, shadow error (if any).
- Shadow errors are caught; primary selection always ships (product B-003).

Config (`WORKFLOW.md` runtime section, all default off):

```yaml
retrieval:
  skills:      { primary: substring, shadow: embedding }
  repo_memory: { primary: substring, shadow: embedding }
```

Promotion = swapping `primary`; no code change.

### 3. Composer enforce mode

Config per activity class:

```yaml
context_composer:
  implement_issue: off | shadow_diff | enforce
```

- `shadow_diff`: after ad-hoc packet assembly in `prompt_packet.rs`, run
  `ContextComposer` with providers fed from the same inputs; persist a
  `composer_shadow_diff` artifact (section-set delta, token delta, manifest);
  ship the ad-hoc packet unchanged.
- `enforce`: the Composer's composed sections replace the corresponding
  ad-hoc packet sections. The packet schema is unchanged (sections are the
  same keys; Non-Goal guards any schema change behind a version bump). The
  selection manifest persists through the GH-1732 provenance path, linked to
  the runtime job's prompt-packet digest. Manifest persistence failure fails
  the composition (product B-005) and surfaces as an activity configuration
  error, which the existing retry taxonomy classifies as non-retryable
  `Configuration`.
- Skills: under `enforce`, `SkillsProvider` (ranked via the retrieval trait)
  contributes the skills section; provider filters out
  `SkillGovernanceStatus::Quarantine | Retired` before ranking.

### 4. Cross-repo memory sharing

- New table `workflow_repo_memory_sharing (repo text primary key,
  share_kinds text[], opted_in_at timestamptz)`; opt-in written by operator
  config sync, never by agents.
- Retrieval for kinds `EnvironmentNote` / `FailureLesson` adds a second
  candidate query over opted-in foreign repos (same 50-row cap, separate
  pool), tags candidates `native_repo: false`.
- Ranking: foreign score is multiplied by a discount factor (config, default
  0.8); ties break native-first (product B-007).
- Rendering: `repo_memory_prompt.rs` labels each foreign record
  `source_repo: <repo>` inside the existing untrusted fence; a foreign record
  reaching rendering without a label is a hard error (product B-008).
- `ValidationCommand` / `ReviewerPattern` kinds are excluded at the query
  level, not by post-filtering.

## Migration / Landing Order

1. Extract `SubstringRetriever` + characterization tests (no behavior
   change; refactor only).
2. Shadow executor + comparison telemetry (flag-gated, default off).
3. Embedding column migrations + write-time embedding + offline backfill job.
4. Composer `shadow_diff` stage in `prompt_packet.rs`.
5. Composer `enforce` for one low-risk activity class; skills return to
   runtime prompts.
6. Primary promotion per surface after telemetry + eval (GH-1768) review.
7. Cross-repo sharing table + retrieval + rendering.

Each step is independently revertible by config flip; steps 3 and 7 add
nullable columns / new tables only (no destructive migration).

## Validation

- `cargo test -p harness-context` — trait, characterization, composer
  shadow/enforce unit tests.
- `cargo test -p harness-workflow memory_retrieval` — ranking, cross-repo
  gating, discount, label enforcement.
- `cargo test -p harness-server workflow_runtime_worker` — packet parity with
  flags off; shadow-diff artifact; enforce-mode manifest linkage.
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all -- --check` per repo gates.

## Open Questions

- Embedding model/provider choice (local model vs API) and dimension `D` —
  decided at step 3; the column and trait are dimension-agnostic until then.
- Whether `context_preview` should preview enforce-mode output once a class
  is enforced (recommended: yes, same code path, keeps preview honest).
