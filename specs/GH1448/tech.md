# Tech Spec

## Linked Issue

GH-1448

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Prompt packet assembly | `crates/harness-workflow/src/runtime/` (dispatcher, activity prompt construction), `WORKFLOW.md` activities | Static per-activity prompts + validation lists | Injection point for the memory section |
| Terminal-state handling | `crates/harness-workflow/src/runtime/reducer/`, `submission.rs` | Reduces runs to terminal states with evidence | Extraction trigger point |
| Runtime storage | `crates/harness-workflow/src/runtime/store.rs`, `store_migrations.rs` | Postgres-backed runtime records | Memory table lives here |
| Prior art (failed) | `crates/harness-skills/` (deletion pending GH-1434 Phase 3), #957/#972 learn pipeline | Standalone learning layer with declaration-execution gap | Anti-pattern to avoid: memory must sit on the traffic path |
| PR feedback | `crates/harness-server/src/workflow_runtime_pr_feedback.rs` | Structured PR feedback handling | Source of reviewer-pattern memory |

## Proposed Design

1. **Schema** — one table in the workflow namespace:
   `workflow_repo_memory(id, repo, activity_class, outcome, kind, payload_json,
   evidence_ref, created_at, last_used_at, use_count)`. `kind` ∈
   {validation_command, failure_lesson, reviewer_pattern, environment_note}.
2. **Extraction** — a post-terminal hook in the reducer path: deterministic
   extractors first (validation commands actually run, failure class from
   activity_result, feedback categories from pr_feedback records). No LLM
   extraction in v1.
3. **Retrieval** — SQL ranking: `activity_class` match, recency decay,
   outcome mix guarantee (at least one failure lesson if any exist), cap K=5
   and a token budget (estimate via chars/4, hard-truncate whole records only).
4. **Injection** — dispatcher appends a fenced `repo-memory` section to the
   prompt packet with a fixed preamble marking it untrusted background
   evidence (SEC-18 posture).
5. **API** — `GET /api/projects/{id}/memory`, `DELETE /api/memory/{id}`
   on existing router surface.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 extraction never blocks | reducer hook error path | unit test: extractor error → run still terminal, error logged |
| P3 bounded injection | retrieval + budget fn | unit test: K and token cap enforced |
| P5 failure lessons surface | ranking SQL | unit test: success-heavy fixture still returns failure record |
| P6 prune takes effect | DELETE handler + retrieval | integration test: delete → next packet excludes record |

## Data Flow

Terminal run evidence (Postgres) → deterministic extractors → memory table →
dispatch-time retrieval → prompt packet section → agent (read-only context).
No external calls; no new storage namespace.

## Alternatives Considered

- Embedding/vector retrieval — rejected for v1 (U-33: lexical + structural
  first; corpus is tiny per repo).
- Reusing harness-skills store — rejected: layer is scheduled for removal and
  previously exhibited the declaration-execution gap.
- Free-text memory notes written by the agent — rejected: injection surface
  and drift risk; structured fields only.

## Risks

- Security: memory is model-influenced content re-entering prompts —
  mitigate by structured fields, untrusted framing, and operator prune.
- Compatibility: prompt packet growth could hit context limits — hard token
  budget; measured in tests.
- Performance: one extra SQL query per dispatch — indexed by (repo,
  activity_class), negligible at current volume.
- Maintenance: stale memories mislead agents — staleness decay + use_count
  telemetry to inform future pruning policy.

## Test Plan

- [ ] Unit tests: extractors (done/failed fixtures), ranking guarantees, budget truncation.
- [ ] Integration tests: end-to-end run → record → next dispatch packet contains section; prune flow.
- [ ] Manual verification: run two sequential issues on one repo; inspect second packet's memory section.

## Rollback Plan

`memory.enabled = false` disables extraction and injection immediately; the
table is additive and can remain in place. No data migration to revert.
