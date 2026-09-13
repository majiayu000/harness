# Architecture Design Recommendations

> Date: 2026-09-12  
> Revised: 2026-09-12 (review pass — tighten severity, priority, and acceptance)  
> Scope: Store criticality, RuntimeKind selection, PR-feedback stop residue, ownership notes  
> Method: Read-only static investigation; recommendations are a **discussion draft**, not a
> copy-execute checklist  
> Status: Discussion draft **and** Phase A–C implemented in-tree (2026-09-12).
> Implementation note: Authority **A** chosen for D3 (`RuntimeKind` selects surface).

## Focused health-fix follow-up

The current health-fix pass additionally passes the preflight-selected backend instance into turn lifecycle, adds merge-review SHA/clean-worktree signal checks, makes active-count queries fail explicitly, and removes the unused startup interceptor stack. See `project-health-audit-2026-09-12.md` for scope, evidence limits, and fresh verification results. Full TaskStore retirement, skill injection, an eval executor, and budget-default changes are not included.

## How to use this doc

- Treat **Fact** sections as the reliable part.
- Treat **Recommendation** sections as proposals that must survive product/design review
  before coding.
- Do **not** implement the whole backlog in document order. Follow the revised priority
  below.
- Working-tree note: some PR-feedback stop scaffolding (`hygiene_convergence.rs`,
  `FeedbackRepairStop` / `MissingBaseline` on `next_feedback_repair_round`) is already
  mid-cut. Re-check those paths before any D4 PR.

Related historical docs (verify before trusting):

- `docs/workflow-runtime-decoupling-plan.md`
- `docs/runtime-submission-identity-contract.md`
- `docs/runtime-operational-reliability-audit.md` (P3 RemoteHost is stale; P6 still relevant)
- `docs/architecture-audit.md`, `docs/bug/2026-03-12-architectural-issues.md`

---

## Revised priority (do this order)

| Order | Work | Closes | Notes |
|---|---|---|---|
| 1 | Invert store criticality | D9 (+ D1 boot half) | `workflow_runtime_store` required for fleet; `tasks` optional |
| 2 | Unify one execution-surface decision rule + single resolver | D3 / P6 | Kind decides surface; do not “fix” by keeping approval→oneshot collapse |
| 3 | Delete or justify remaining invalid stop / fake blocker writes | D4 | Question `MissingBaseline` / blocker-presence stops — do not only relocate them |
| 4 | TaskStore retirement (reads → delete modules) | D1 remainder | Separate track after (1); vocabulary rename can trail |
| — | Ownership / multi-site policy | D2 / D5 | Opportunistic: unify **semantics**, keep role-appropriate checks |
| — | Child-start transaction window | D8 | Needs failure proof under lease loss/retry before mandating one big txn |
| — | Extract `harness-db` from core | ~~D6~~ | **Out of scope** — `db-postgres` is already optional |

---

## Finding index (revised)

| ID | Severity | Priority | One-line | Action posture |
|---|---|---|---|---|
| D9 | High | P1 | Truth store optional; legacy TaskStore critical | Fix now |
| D1 | High | P1 boot / P4 full retire | TaskStore still required at boot; reads/vocab remain | Criticality with D9; retirement separate |
| D3 | High | P2 | Multiple RuntimeKind→backend selection paths | Fix selection rule first, then code |
| D4 | Medium | P3 | Hollow repair-stop / fake blocker residue after round-cap removal | Clean invalid stops; do not preserve empty gates |
| D2 | Low–Med | Later | Server holds PR-feedback admission beside workflow decisions | Unify shared predicates when touched; not a rewrite driver |
| D5 | Low–Med | Later | Same product rule appears in sweeper / merge / prompt / reducer | Unify judgment semantics; keep necessary role checks |
| D8 | Low until proven | Evidence-gated | Side effects before parent completion | Prove bad outcome, then decide |
| D7 | Low | Opportunistic | Orphans (`parallel_dispatch`, dead TaskStore helpers) | Delete when adjacent |
| D6 | Dropped | — | Core “DB coupling” | Overstated; optional feature already |

---

## D9 — Truth store optional, legacy store critical

### Fact

- Admissions write only to the workflow runtime.
- `workflow_runtime_store` still starts as optional; some reads fail open to empty.
- `tasks` / TaskStore remains **critical** at boot (`http/builders/storage.rs`, `http/init.rs`).

### Impact

Fleet can look healthy with an empty runtime view while a legacy store that no longer
receives admissions is mandatory. This is the concrete dependency inversion.

### Recommendation

1. Mark `workflow_runtime_store` required for fleet `serve` (or for any route that admits
   or lists submissions).
2. Make TaskStore **optional** (serve continues if absent or empty).
3. Health/degraded responses must name which store failed (align with reliability audit P4).

Do **not** couple this PR to full TaskStore deletion.

### Acceptance

- Fleet serve without workflow runtime store fails closed.
- Fleet serve without TaskStore succeeds.
- No silent empty “legacy queue” substitute for missing runtime state on operator APIs
  that claim to show live work.

---

## D1 — Incomplete task → workflow cutover

### Fact

- Admission write path to TaskStore is gone; public API is workflow submissions.
- AppState still holds both stores; TaskStore still used for boot recovery/retention,
  some read APIs, and vocabulary (`Task*`, `task_id` alias, UI `["tasks"]` keys).

### Impact

Dual mental model. Full retirement is real work but **not** the same urgency as D9.

### Recommendation (split)

**Now (with D9):** optional TaskStore + stop aborting serve on it; rehome only boot
dependents that currently force a critical open (catalog pool, reconcile inputs).

**Later (separate track):** rewrite remaining reads to workflow projections; disable
legacy retention/artifact sink; delete `task_runner` / `task_db`; optional vocabulary
rename.

**Do not** rebuild TaskStore as a dual-write projection of workflow state.

### Acceptance (P1 slice)

- Same as D9 for boot criticality.

### Acceptance (retirement track)

- Server can run with no TaskStore module/table.
- Operator/intake/health counts come from runtime projections only.
- Grep for production `TaskStore` is gone (tests may trail briefly).

---

## D3 — RuntimeKind / execution-surface selection

### Fact

- One trait (`AgentBackend`); registry slots: oneshot / control / turn factory.
- At least three selection paths disagree (`agent_backend_for_runtime_kind`,
  `agent_backend_for_attempt`, `force_code_agent_for_runtime_turn` + lifecycle
  re-resolve by **agent name**).
- `force_code_agent_for_runtime_turn` collapses both Codex kinds to oneshot when
  approval is non-interactive (`runtime_turn_control.rs`).
- Continues reliability audit P6 (#1204).

### Impact

Preflight can inspect a different backend than execution. Kind labels do not reliably
name the spawn contract.

### Design question (must answer before coding)

**What is the single authority for execution surface?**

Candidates (pick one; document the choice in the PR):

| Authority | Meaning |
|---|---|
| A. `RuntimeKind` only | `CodexExec` = oneshot, `CodexJsonrpc` = turn always. Approval policy configures the turn/oneshot agent; it does **not** switch surface. |
| B. Explicit profile field | e.g. `execution_surface: oneshot \| turn` on the runtime profile; kind becomes a label/family only. |
| C. Approval-driven (status quo intent) | Interactive approvals require turn; non-interactive may use oneshot. Then **kind names must not claim otherwise** — rename or stop advertising Jsonrpc when collapsed. |

The earlier draft matrix (Jsonrpc defaults to turn but non-interactive force-oneshot)
is **rejected as a “fix”**: it preserves the original lie.

Correction / contract path: resolve against the **same** surface the job will run, or
document a separate contract-only backend without pretending it is the job surface.

### Recommendation

1. Choose A, B, or C in design review (prefer **A** unless product requires approval to
   change process family).
2. Implement **one** resolver used by preflight, contracts, and lifecycle; pass the
   `Arc` through — no second select-by-name.
3. Add invariant tests for the chosen rule (not for the rejected hybrid matrix).

### Acceptance

- Written decision: which input selects surface; confirmation that approval does or
  does not change it.
- One code path selects the backend for a job attempt.
- Preflight backend identity matches executed backend.
- No test that asserts contradictory CodexExec/Jsonrpc mappings.

---

## D4 — Hollow repair-stop residue

### Fact

- Round cap / progress comparison was removed; repair rounds are telemetry
  (`next_feedback_repair_round` now only increments).
- Hygiene persistence may still write a fixed `feedback_repair_blocker_count: 1`.
- Recovery still special-cases `pr_hygiene_convergence` stop metadata in places.
- Working tree already deleted some convergence modules; re-audit before changing.

### Impact

Empty or presence-only gates can still **block** repair or recovery even though they no
longer measure progress. Relocating the same gate to the reducer does not fix that.

### Design question

Is **MissingBaseline** / “blocker field must exist” still a product stop?

- Today the old blocker count is (or was) used mainly as presence, not as progress.
- If nothing consumes that presence for a real safety property, **delete the stop** and
  stop writing fake baselines.
- If some recovery or observability path still needs a baseline, define what it means
  with real measurements — do not keep a hollow stop “for completeness.”

### Recommendation

1. Inventory remaining writers/readers of `feedback_repair_blocker_count` and hygiene
   convergence `last_stop` sources.
2. Remove stops that only check presence without a justified failure mode.
3. Remove unused parameters, fake `json!(1)` writes, and recovery branches that exist
   only for deleted policy.
4. Do **not** treat “one check at reducer” as done if the check itself is unjustified.

### Acceptance

- No stop whose sole condition is “previous blocker count field missing” unless a
  documented product reason remains.
- No hardcoded blocker baseline on hygiene request.
- Repair can proceed when findings exist even if telemetry counters are absent.
- Tests that encoded oscillation/round-limit/MissingBaseline folklore are updated or
  removed.

---

## D2 — Workflow vs server ownership (tone down)

### Fact

Workflow owns reducer/validator/store/recovery decisions; server owns HTTP, GitHub I/O,
spawn, sweeps, and still hosts large `workflow_runtime_*` admission/worker code.
Some constants (definition ids) are duplicated.

### Impact

Drift risk on shared identifiers and candidacy predicates — real but **not** a reason
to relocate the worker or delete server admission.

### Recommendation

When touching these paths:

- Prefer workflow exports for definition ids / shared candidacy helpers.
- Keep sweeper, merge gate, and prompt assembly in server — they are different roles
  (drive work / enforce merge / instruct agent).

### Acceptance

- No private redefinition of a workflow-owned definition id in a change that already
  touches that string.
- No mega-move of `workflow_runtime_worker` into `harness-workflow`.

---

## D5 — Multi-site rules (tone down)

### Fact

Review-thread / readiness concerns appear in sweeper, auto-merge, reducer, and prompts.
Activity success-vs-blockers appear in server contracts and reducer.

### Impact

**Semantic drift** is the risk, not the mere existence of multiple call sites.

### Recommendation

- Extract or share the **judgment** (e.g. “unresolved threads count as not merge-ready”)
  so sweeper/merge/reducer disagree less.
- Keep role-appropriate actions:
  - sweeper: may *trigger* repair when the shared predicate says work is needed
  - auto-merge / reducer: *enforce* readiness at their boundaries
  - prompt: *instruct* the agent; not the sole authority
- Do **not** delete a necessary check only because another layer also mentions it.

### Acceptance

- Shared predicate or documented single semantic definition when a PR changes the rule.
- Each layer’s remaining check maps to a stated role.

---

## D6 — Core / Postgres — dropped as a drive finding

### Fact

`harness-core` exposes optional `db-postgres` (`Cargo.toml`: `default = []`,
`db-postgres = ["dep:sqlx"]`). Non-DB consumers already depend on core without sqlx.

### Impact

Earlier “split harness-db” advice overstated coupling.

### Recommendation

**Do nothing.** No new crate. No priority. Mention only if a future consumer needs a
different packaging story.

---

## D7 — Residue (unchanged, opportunistic)

| Item | Action |
|---|---|
| `parallel_dispatch` with no production importer | Delete when convenient |
| Dead TaskStore admission helpers / recovered-PR validators | Delete with retirement track |
| Stale audit P3 RemoteHost | Mark done/stale in that doc when edited |

---

## D8 — Side effects before parent completion (evidence-gated)

### Fact

Some activities (e.g. child start) persist side effects before lease-owned parent
completion. Replay/idempotency paths exist. Reliability audit P2 described a window.

### Impact

A **window** is not automatically a proven production defect. Without a demonstrated
incorrect state under lease loss or retry, mandating one large transaction is premature.

### Recommendation

1. Before redesign: write down a concrete failure scenario (lease loss / double claim /
   retry) and show the bad durable state under current idempotent replay.
2. If proven: tighten that activity family’s commit boundary.
3. If not proven: leave as known window; do not schedule a standalone mega-refactor.

### Acceptance (only if pursuing a fix)

- Reproducing test or recorded failure showing incorrect state after lease loss/retry
  despite current replay.
- Fix closes that scenario; no new reconciliation daemon.

---

## Phased roadmap (revised)

### Phase A — Store criticality (P1)

- D9 + D1 boot slice: require workflow runtime store; optional TaskStore.
- Acceptance per D9.

### Phase B — Execution surface truth (P2)

- Design choice A/B/C for D3; one resolver; invariant tests.
- Do not land the rejected hybrid “Jsonrpc + approval force oneshot” matrix as the fix.

### Phase C — Invalid stop cleanup (P3)

- D4 inventory; remove unjustified presence stops and fake blocker writes.
- Simplify recovery branches tied to deleted policy.

### Phase D — TaskStore retirement (separate)

- Reads → delete modules/tables → optional rename.
- Independent of A/B/C sequencing except A should land first.

### Opportunistic

- D2/D5 semantic sharing when editing those rules.
- D7 deletes when adjacent.
- D8 only after failure proof.

---

## Anti-goals

- Do not treat this doc as a copy-execute remediation script.
- Do not reintroduce TaskStore as a workflow projection.
- Do not “fix” D3 by keeping approval-driven surface switching while claiming kind names
  are truthful — pick an explicit authority instead.
- Do not preserve MissingBaseline / presence-only stops just by moving them to the reducer.
- Do not delete sweeper/merge/prompt checks solely for de-duplication theater.
- Do not split `harness-core` for DB packaging.
- Do not mandate a child-start mega-transaction without a proven failure.
- Do not big-bang move the runtime worker into `harness-workflow`.

---

## Suggested issue / PR titles (revised)

1. `fix(server): require workflow_runtime_store; make TaskStore optional at serve`
2. `fix(runtime): single execution-surface resolver (document kind vs approval authority)`
3. `refactor(pr-feedback): remove unjustified repair stops and fake blocker baselines`
4. `refactor(server): retire TaskStore reads after optional-store cutover` (separate)
5. (Only if proven) `fix(runtime): close child-start lease-loss failure <scenario>`

---

## Verification guidance

Per `AGENTS.md`: smallest package check during work; filtered tests for behavior; full
workspace clippy before push; never local `cargo test --workspace` unless requested.

| Focus | Prefer |
|---|---|
| D9/D1 | Serve/boot tests: runtime store required, TaskStore optional |
| D3 | Executor/lifecycle tests: one resolver, chosen matrix only |
| D4 | Repair proceeds without telemetry baseline; no fake `blocker_count: 1` |

---

## Appendix — Evidence anchors

| Claim | Anchor |
|---|---|
| Dual stores on AppState | `crates/harness-server/src/http/state.rs` |
| Critical tasks store | `crates/harness-server/src/http/builders/storage.rs` |
| Workflow-only admission | `crates/harness-server/src/services/execution/mod.rs` |
| Kind map | `.../workflow_runtime_worker/runtime_profile.rs` |
| Preflight selection | `.../workflow_runtime_worker/executor/mod.rs` |
| Approval force-oneshot | `.../workflow_runtime_worker/runtime_turn_control.rs` |
| Repair rounds = telemetry | `crates/harness-workflow/src/runtime/pr_feedback.rs` |
| Fake hygiene blocker write (if still present) | `.../workflow_runtime_pr_feedback/persistence.rs` |
| Optional `db-postgres` | `crates/harness-core/Cargo.toml` |
| Identity contract | `docs/runtime-submission-identity-contract.md` |
| Prior P6 | `docs/runtime-operational-reliability-audit.md` |

---

## Review changelog

| Feedback | Doc change |
|---|---|
| D3 matrix preserved the bug | Rejected hybrid matrix; require explicit surface authority (A/B/C) |
| D4 MissingBaseline unjustified | Stop questioning product need; do not “fix” by relocating only |
| D6 overstated | Dropped as drive finding; optional feature noted |
| D8 unproven | Demoted to evidence-gated; proof required before txn merge |
| D2/D5 overweighted | Tone down; unify semantics, keep role checks |
| Preferred sequence | Criticality + execution path → invalid stops → TaskStore retire; no core split |
