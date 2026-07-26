# Semantic Retrieval and Context Enforcement — Analysis

> Date: 2026-07-26
> Scope: every mechanism that selects knowledge (skills, repo memory, context
> items, routing) for injection into agent prompts.
> Method: read-only inspection at commit `81c78255`. Line numbers cited were
> verified against that commit.
> Linked issue: GH-1769. Spec packet: `specs/GH1769/`.

## Executive Summary

Harness has accumulated four distinct knowledge-selection surfaces — skill
matching, repo memory retrieval, context composition, and complexity routing —
and every one of them selects by substring, recency, or token counting. There
is no embedding, no reranking, and no learned signal anywhere in the retrieval
path. Worse, the two most sophisticated pieces are disconnected from
production: the Context Composer (budgets, dedupe, degradation ladder,
manifest) is reachable only from the `context/preview` RPC, and after the
legacy task layer removal, skill matching has **no runtime call site at all**.
The consequence is that the knowledge the system works hardest to accumulate —
skills with EMA quality scores and governance states, per-repo failure
lessons — reaches live prompts either crudely or not at all.

Retrieval precision is the ceiling on knowledge reuse. This document
inventories each surface with evidence, explains the compounding cost across a
multi-repo deployment, and lays out a graduated design: pluggable retrieval
with shadow comparison, Composer enforce mode behind a per-activity flag, and
opt-in cross-repo memory with provenance.

## Inventory of Retrieval Surfaces

### 1. Skill matching — substring, and currently orphaned

`crates/harness-context/src/providers.rs` implements `SkillsProvider`. Its
relevance test lowercases the prompt and checks whether any trigger pattern is
a literal substring:

- `providers.rs:71` — `.to_lowercase()` on the prompt.
- `providers.rs:76-82` — a skill matches when `trigger_patterns.is_empty()`
  (always injected) or `.any(|pattern| prompt.contains(&pattern.to_lowercase()))`.
- `providers.rs:93` — relevance is a coarse constant depending only on whether
  trigger patterns exist, not on match quality.

`crates/harness-core/src/prompts/context.rs:82`
(`build_matched_skills_section`) renders the matched set into a prompt
section.

**Call-site reality (verified at `81c78255`):** the only consumer of
`SkillsProvider` and `build_matched_skills_section` outside their defining
crates is `crates/harness-server/src/handlers/context.rs:115` — the
`context/preview` RPC. The legacy `augment_prompt_with_skills` path was
removed with the legacy task layer, and the live runtime prompt packet
(`crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs`) injects
`repo_memory` (`prompt_packet.rs:88-89`) but has no skills section. The skill
store's governance machinery — `SkillGovernanceStatus`
(`crates/harness-skills/src/store.rs:14`), EMA `quality_score`,
`canary_ratio` (`store.rs:43`) — governs a library that runtime prompts never
see. Skills currently reach agents only through agent-native discovery (e.g.
Claude CLI's own skill loading), which Harness neither ranks nor governs.

Two defects, then: the matcher is a substring test, and the matcher is dead.

### 2. Repo memory retrieval — recency-ordered, repo-fenced

`crates/harness-workflow/src/runtime/memory_retrieval.rs` is real and wired
into the live prompt packet, but its ranking is minimal:

- `memory_retrieval.rs:7-9` — top-5 records, 800-token budget, 50-candidate
  SQL fetch.
- `memory_retrieval.rs:44-52` — the query:
  `WHERE repo = $1 ... ORDER BY CASE WHEN activity_class = $2 THEN 0 ELSE 1
  END ASC, created_at DESC LIMIT $3`. Exact-match-on-activity-class then
  recency; no content relevance of any kind.
- `memory_retrieval.rs:85-89` — in-process reranking
  (`rank_repo_memory_candidates`) re-applies the same two keys.
- `memory_retrieval.rs:47` — `WHERE repo = $1` hard-fences memory by repo
  string. A `FailureLesson` learned in one repository can never inform work in
  another.

The stored kinds (`ValidationCommand`, `FailureLesson`, `ReviewerPattern`,
`EnvironmentNote` in `runtime/repo_memory.rs`) are exactly the categories
where cross-repo transfer pays: the deployment runs webhook intake for ~25
repositories that share toolchains, reviewers, and infrastructure quirks.
Today each repo re-learns every lesson from scratch, and within a repo the
freshest five records win regardless of whether they match the task at hand.

### 3. Context Composer — built, budgeted, and stuck in shadow

`crates/harness-context/src/composer.rs` (652 lines) implements what a
state-of-the-art context assembler needs: per-class token budgets, duplicate
suppression, a degradation ladder that trims lower-priority classes first, and
a manifest recording what was selected and why. Providers exist for Rules,
Skills, Contract, ExecPlan, TaskBrief, GcDrafts, Static, and Error content.

Its only call site outside the crate is
`crates/harness-server/src/handlers/context.rs:17` (`context_preview`) — the
shadow-mode RPC. Zero execution-path call sites. Live prompt assembly instead
happens ad hoc in `workflow_runtime_worker/prompt_packet.rs`, which
concatenates workflow config, runtime contract, command input, and repo memory
into the `harness.runtime.prompt_packet.v1` JSON without class budgets or
dedupe. The spec that introduced the Composer
(`specs/context-composer/product.md`) explicitly staged shadow-first; the
enforce stage never landed.

The recently merged GH-1732 provenance spec (`specs/GH1732/`) records *which*
sources were selected into a prompt packet. It is the natural audit substrate
for an enforce-mode Composer: the manifest the Composer already emits is the
provenance record GH-1732 wants.

### 4. Complexity routing — token counting

`crates/harness-server/src/complexity_router.rs:41` (`classify`) counts
distinct file-path-shaped tokens in the prompt (`complexity_router.rs:36,42`)
and maps the count to a complexity tier. It is deterministic and cheap, but it
routes model/agent selection on a proxy (mentioned file count) that neither
correlates reliably with difficulty nor adapts from outcomes.

## Why Retrieval Precision Bounds Everything Else

1. **Knowledge reuse is the product's compounding asset.** The harness's
   differentiation over raw agent CLIs is accumulated, governed knowledge —
   skills, lessons, reviewer patterns. Each percentage point of retrieval
   precision/recall directly scales how much of that asset reaches the agent
   within the fixed token budget (800 tokens for memory; 2% skills budget on
   the Codex side per `activity_result.rs:16`).
2. **Substring matching fails in both directions.** It misses paraphrases
   ("upgrade the toolchain" never matches trigger "rust version bump") and
   false-fires on incidental mentions, and it cannot rank partial matches —
   `providers.rs:93` assigns one constant relevance to all matched skills.
3. **Recency is anti-signal for stable lessons.** `EnvironmentNote`s
   ("worktree layout breaks `gh pr merge --delete-branch`") stay true for
   months; `ORDER BY created_at DESC` evicts them the moment five newer
   records exist in the same activity class.
4. **Un-budgeted assembly wastes the window.** Without the Composer's dedupe
   and degradation ladder, live packets carry redundant sections while
   relevant knowledge is truncated by coarse limits.
5. **25-repo deployment multiplies every miss.** Repo-fenced memory means the
   same failure is re-discovered up to 25 times, each discovery costing agent
   turns and tokens.

## Design

### D1. Pluggable retrieval with shadow comparison

Introduce a retrieval trait covering both surfaces (skills, repo memory):
candidate fetch → score → rank, with two implementations: the current
substring/recency logic (baseline) and an embedding scorer (pgvector on the
existing Postgres, or an in-process model; no external vector service). Both
run in shadow: the baseline's selection ships to the agent, both selections
are logged with overlap/rank-divergence telemetry. Promotion to primary is a
config flip once shadow data shows parity-or-better on the eval flywheel
(GH-1768) — measuring retrieval quality without replayable evals reduces to
anecdote, so GH-1768 is a soft dependency for promotion, not for landing.

### D2. Composer enforce mode, per-activity flag

Wire `ContextComposer` into `prompt_packet.rs` behind a per-activity flag
(default off). Stage 1: shadow-diff — compose in parallel, log the manifest
and the byte/token delta versus the ad-hoc packet, ship the ad-hoc packet.
Stage 2: enforce for one low-risk activity class, manifest recorded via the
GH-1732 provenance path. Stage 3: graduate per activity on operator review of
diffs. This also resurrects skills injection on the runtime path — the
Composer's `SkillsProvider` becomes the (single) live skill matcher, ranked by
D1.

### D3. Cross-repo memory with provenance

Add an opt-in share class for memory kinds where transfer is safe
(`EnvironmentNote`, `FailureLesson`): retrieval may include foreign-repo
records when the owning repo opts in, every injected foreign record carries
`source_repo` provenance in the prompt section (extending the untrusted-data
preamble in `repo_memory_prompt.rs`), and ranking discounts foreign records
relative to native ones. `ValidationCommand` and `ReviewerPattern` stay
repo-fenced — they encode repo-specific truth.

## Risks and Alternatives

- **Embedding cost/latency.** Mitigation: embed at write time (one embedding
  per memory record / skill), query-time embedding is a single call; candidate
  set stays SQL-prefiltered (50 rows) so reranking is cheap. Alternative: a
  pure lexical upgrade (BM25 via Postgres full-text search) captures much of
  the win with zero new dependencies; the trait makes this a third pluggable
  implementation, and it is the fallback if embedding shadow data disappoints.
- **Composer enforce regressions.** Mitigation: shadow-diff stage plus
  per-activity flag bounds the blast radius to one activity class; rollback is
  a config flip.
- **Cross-repo poisoning.** A malicious or wrong lesson in one repo could
  steer agents in another. Mitigation: opt-in per repo, provenance-labeled
  injection inside the existing untrusted fence, and foreign records excluded
  from `ValidationCommand`-class trust.

## Recommended Order

1. D1 trait extraction + shadow telemetry (no behavior change).
2. D2 stage 1 shadow-diff (no behavior change; produces evidence).
3. D2 stage 2 enforce on one activity; skills return to runtime prompts.
4. D1 promotion decision from shadow + eval data.
5. D3 cross-repo opt-in.
