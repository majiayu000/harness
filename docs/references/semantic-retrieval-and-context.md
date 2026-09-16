# Retrieval and Context Integration

> Verified: 2026-09-16 against `4c2459ca`.
> Linked issue: [GH-1769](https://github.com/majiayu000/harness/issues/1769).
> Scope: implemented selection behavior and the evidence needed for further work.

## Current behavior

PR #1924 delivered shared deterministic lexical scoring. Repo-memory retrieval
uses it in production. Semantic retrieval, production skill selection through
Harness, and production Context Composer integration remain incomplete.

| Surface | Implemented behavior | Execution boundary |
| --- | --- | --- |
| Shared retrieval | `KnowledgeRetriever`, `LexicalKnowledgeRetriever`, and primary/shadow comparison | `crates/harness-core/src/retrieval.rs`; shadow comparison has tests but no production caller |
| Skills | Lexical trigger/content relevance and deterministic ordering | `SkillStore::match_prompt` in `crates/harness-skills/src/store.rs` has test callers; `SkillsProvider` in `crates/harness-context/src/providers.rs` is used by context preview |
| Repo memory | Same-repository candidate admission, lexical task relevance, activity preference, use count, recency, and token/count budgets | `crates/harness-workflow/src/runtime/memory_retrieval.rs`; called by the workflow worker |
| Context Composer | Class budgets, deduplication, degradation, and a selection manifest | `crates/harness-context/src/composer.rs`; the server constructs it in `handlers/context.rs` for `context/preview` |
| Complexity routing | Deterministic file-path-count heuristic | `crates/harness-server/src/complexity_router.rs`; no measured routing improvement is established here |

### Repo-memory selection and production injection

`retrieve_repo_memory_records_for_task` admits up to 50 same-repository records,
with SQL ordering by activity-class match and recency. It then ranks the admitted
set by activity match, lexical relevance to the activity and task, use count,
recency, and stable ID. Selection applies the default five-record, 800-estimated-
token budget and the existing failure-lesson mix policy.

This distinction matters: content ranking is implemented, but it cannot recover a
relevant record excluded from the initial 50 candidates. Whether that admission
bound causes an actual retrieval failure requires evidence from a concrete task.

`workflow_runtime_worker/executor/mod.rs` calls
`repo_memory_for_prompt_packet` from `repo_memory_prompt.rs` when memory is
enabled, then passes the records to `build_runtime_prompt_packet`. The prompt
packet's `context_provenance.rs` records selected memory sources; that provenance
does not establish that the source claim is correct or the selection is optimal.

### Skills and Context Composer

Skill matching is no longer substring-only. Both the skill store and the
Composer's skill provider use the shared lexical retriever. However, a library
implementation or successful preview is not evidence of runtime injection.
The inspected server has no production agent-prompt caller for this ranked
skill-selection path and no execution-path caller of `ContextComposer`.

The live workflow packet is assembled in
`crates/harness-server/src/workflow_runtime_worker/prompt_packet/`. Agent-native
skill discovery is a separate surface and does not prove that Harness selected
or governed those skills.

## Remaining work and restart criteria

GH-1769 remains deferred. The previous runtime audit did not reproduce a concrete
retrieval failure, and this source inspection does not establish one. The absence
of embeddings alone is not a defect requiring a new subsystem.

The next useful evaluation should preserve one real task, its eligible candidate
set, the expected relevant records with reviewer rationale, the current selected
records, and the resulting prompt. Classify the failure before changing behavior:

1. Candidate admission excluded the needed record.
2. Ranking placed the record below the count/token budget.
3. Selection was correct but the execution path did not inject it.
4. The task did not have useful stored knowledge to retrieve.

Only after a reproducible failure should a bounded change be compared with the
current lexical baseline. Measure selection quality and prompt size, and use
GH-1768's task-outcome evidence when available. Production Composer integration,
semantic ranking, cross-repository memory, and learned routing are distinct
changes; none is authorized merely because another surface has a failure.

No embedding provider, vector store, cross-repository sharing policy, or new
configuration flag is selected by this document. Earlier proposals in
`specs/GH1769/` require reassessment against the implemented lexical baseline
and a measured failure before implementation.
