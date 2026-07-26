# Product Spec

## Linked Issue

GH-1769

## User Problem

Every mechanism that selects accumulated knowledge for agent prompts ranks by
literal substring match, recency, or token counting. Skill matching is a
lowercase `contains` test whose only remaining call site is the
`context/preview` RPC — after the legacy task layer removal, governed skills
reach no runtime prompt at all. Repo memory retrieval orders by activity-class
equality then `created_at DESC` inside a hard `WHERE repo = $1` fence, so
stable environment lessons are evicted by newer noise and nothing learned in
one repository ever helps another. The Context Composer's budgets, dedupe, and
degradation ladder run only in preview shadow mode while live packets are
assembled ad hoc.

Operators accumulate skills and memory that the system then fails to deliver
to the agents doing the work.

## Goals

- Make knowledge retrieval pluggable: one trait, with the current
  substring/recency behavior and a semantic (embedding) scorer as
  interchangeable implementations.
- Run semantic retrieval in shadow first, with telemetry that quantifies
  divergence from the baseline before any behavior change.
- Give the Context Composer an enforce mode, gated per activity class, with a
  recorded selection manifest, and restore skill injection to the runtime
  prompt path through it.
- Allow explicitly opted-in cross-repo sharing for transfer-safe memory kinds
  (`EnvironmentNote`, `FailureLesson`) with visible source-repo provenance.

## Non-Goals

- No external vector database or retrieval service dependency; embedding
  storage uses Postgres (pgvector) or an in-process index only.
- No change to the `harness.runtime.prompt_packet.v1` schema without an
  explicit schema-version bump.
- No removal of the substring retrieval path until shadow telemetry
  demonstrates parity or better for the semantic path.
- No cross-repo sharing of `ValidationCommand` or `ReviewerPattern` memory
  kinds.
- No changes to skill governance semantics (EMA scoring, canary, quarantine)
  beyond connecting governed skills to a live injection path.
- No learned or model-based complexity routing in this spec.

## User-Visible Behavior

1. **B-001:** Skill and repo-memory retrieval each route through one
   retrieval interface with named implementations. The default primary
   implementation reproduces current selection byte-for-byte for identical
   inputs.
2. **B-002:** When shadow retrieval is enabled, every retrieval executes the
   primary and shadow implementations; only the primary's selection is
   injected into any prompt. Shadow results affect telemetry only.
3. **B-003:** Each shadowed retrieval emits a comparison record containing:
   surface (skill | repo_memory), both implementation names, both ranked
   selection lists (ids and scores), overlap count, and rank divergence. A
   shadow implementation failure is recorded and never fails the retrieval.
4. **B-004:** Composer enforce mode is configured per activity class and
   defaults to off. In shadow-diff stage, the Composer runs alongside ad-hoc
   assembly, the packet delta is recorded, and the ad-hoc packet ships. In
   enforce stage, the Composer's output becomes the shipped packet section
   set.
5. **B-005:** Every enforce-mode composition records a selection manifest
   (item class, source identity, order, selection reason, token cost,
   degradation actions) linked to the runtime job's existing prompt-packet
   evidence. A manifest that cannot be recorded is an error for that
   composition, not a silent skip.
6. **B-006:** Under enforce mode, governed skills are matched and injected
   into runtime prompts via the Composer's skill provider; skills with
   governance status `Quarantine` or `Retired` are never injected.
7. **B-007:** Cross-repo memory retrieval returns foreign-repo records only
   for kinds `EnvironmentNote` and `FailureLesson`, only from repositories
   that have opted in to sharing, and ranks foreign records below equally
   scored native records.
8. **B-008:** Every injected foreign-repo memory record is rendered inside
   the existing untrusted repo-memory fence and labeled with its source
   repository. An unlabeled foreign record is a composition error.
9. **B-009:** Disabling shadow retrieval, enforce mode, or cross-repo sharing
   returns the affected surface to its prior behavior with no data migration.
10. **B-010:** Retrieval implementation choice, shadow enablement, enforce
    stage, and sharing opt-ins are visible in configuration inspection output
    so an operator can determine the active retrieval posture without reading
    logs.

## Acceptance Criteria

- [ ] A characterization test proves the baseline implementation reproduces
      current skill matching (`providers.rs` substring semantics) and current
      repo-memory ordering (`memory_retrieval.rs` activity-class-then-recency)
      for fixed fixtures.
- [ ] Shadow-mode tests prove: primary-only injection, comparison record
      emission with overlap and divergence fields, and shadow-failure
      isolation (retrieval succeeds, failure recorded).
- [ ] Composer shadow-diff tests prove the ad-hoc packet ships unchanged
      while the diff record is persisted.
- [ ] Enforce-mode tests prove manifest recording, linkage to prompt-packet
      evidence, and hard failure on unrecordable manifest.
- [ ] A test proves `Quarantine`/`Retired` skills are excluded from
      enforce-mode injection.
- [ ] Cross-repo tests prove: kind allowlist enforcement, opt-in gating,
      foreign-below-native ranking, provenance labeling inside the untrusted
      fence, and rejection of unlabeled foreign records.
- [ ] A rollback test proves each flag independently restores prior behavior.
- [ ] Existing prompt-packet schema tests pass unchanged when all flags are
      off.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Empty candidate sets, empty trigger patterns, and empty prompts flow through both implementations; B-001 fixes baseline semantics for the always-inject empty-trigger case. |
| Error and failure paths | B-003 (shadow failure isolation), B-005 (manifest failure is loud), B-008 (unlabeled foreign record is an error). |
| Authorization / permission | B-007: sharing requires explicit per-repository opt-in; no ambient cross-repo read. |
| Concurrency / race / ordering | Shadow comparison records are per-retrieval and append-only; no shared mutable ranking state between concurrent retrievals. |
| Retry / repetition / idempotency | Retrieval is read-only and idempotent; repeated composition for the same job produces a new manifest linked to the same packet evidence. |
| Illegal state transitions | Enforce stages progress off → shadow-diff → enforce per activity; configuration cannot express enforce without manifest recording. |
| Compatibility / migration | B-009: all features are flag-gated with off defaults; no schema change without version bump (Non-Goals). |
| Degradation / fallback | Shadow implementation failure degrades to primary-only with a recorded event, never silently. |
| Evidence and audit integrity | B-005/B-010: manifests link to existing runtime-job evidence; retrieval posture is inspectable. |
| Cancellation / interruption / partial completion | A composition interrupted before manifest persistence ships nothing; there is no partially-enforced packet. |

## Edge Cases

- A skill with empty `trigger_patterns` (currently always injected) under
  semantic scoring.
- Two memory records with identical scores but different repos (native must
  win).
- A repo opts out of sharing after its records were already retrieved
  elsewhere (next retrieval excludes them; no retroactive prompt rewrite).
- The embedding column is absent for old records (fall back to baseline score
  for those candidates, recorded in telemetry).
- Shadow and primary implementations disagree on candidate-set size limits.
- An activity class configured for enforce mode receives a prompt with zero
  matched items (compose an empty section set with a manifest, not an error).

## Rollout Notes

Land in the order: trait extraction with characterization tests → shadow
telemetry → Composer shadow-diff → enforce on one low-risk activity class →
semantic-primary promotion → cross-repo opt-in. Promotion of the semantic
implementation to primary requires shadow telemetry review and replayable eval
evidence (GH-1768 flywheel); until then the substring path remains primary.
Reverting any stage is a configuration flip; no data migration is required in
either direction. Embedding backfill for existing records may run as an
offline job at any point after trait extraction.
