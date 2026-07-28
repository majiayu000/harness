# Harness Backlog Cleanup Context

## Task statement

Review, repair, verify, publish, and safely merge every actionable open pull
request in `majiayu000/harness`; then triage every open issue, implement the
remaining repository-scoped work, and close only items that are demonstrably
complete, duplicated, obsolete, or externally blocked.

## Desired outcome

- No open pull request remains unreviewed or ambiguously abandoned.
- Every actionable open issue is either implemented and verified or has a
  precise external blocker.
- Merges preserve the protected-branch technical checks and use exact-head
  matching.
- Final evidence includes fresh tests, lint, formatting, review, ruleset, and
  open-backlog audits.

## Known facts and evidence

- Merged: #1824, #1826, #1830, and #1838.
- `origin/main` is currently `54ebb303d98adb084697bb330bdfe204a68f669a`.
- Open PRs: #1812, #1814, #1821, #1827, #1833, #1834, #1835, and #1836.
- #1833 has a correct centralized builder but its previous 2,742-line
  pseudo-resolver failed independent review. It is being replaced with a
  crate-local Clippy boundary and a narrow accidental-drift structural guard.
- #1814 failed security review for secret exposure, container authentication,
  cleanup, Docker endpoint propagation, adapter reuse, git alternates, and
  proxy userinfo. An isolated repair lane is active.
- #1836 passed an independent exact-head review and awaits integration after
  #1833.
- #1834 depends on #1836 and is under an independent read-only exact-head
  review.
- #1821 has a local documentation fix at `4a909381` and awaits mainline
  integration and review.
- #1835 is broad, conflicting, and must not merge until its useful work is
  preserved through #1836, #1834, and the remaining #1818-#1820 subtasks.
- #1812 and #1827 are stale SpecRail-era remediation specifications. Their
  useful acceptance findings must be implemented before those PRs are closed.
- The repository currently has 63 open issues, including the ASC dependency
  chain and architecture/security follow-ups.

## Constraints

- Conversation is Chinese; repository artifacts, commit messages, and PR text
  are English.
- Search before creating files or abstractions.
- No force push, no test weakening, no silent error swallowing, and no
  hardcoded secrets.
- Parallel writers have explicit disjoint ownership and isolated worktrees.
- Files must remain at or below the repository's 800-line hard ceiling, subject
  only to documented repository exceptions.
- Before commit: `cargo fmt --all` and `cargo fmt --all -- --check`.
- Before push: `cargo clippy --workspace --all-targets -- -D warnings`.
- Behavior changes require affected package tests; shared workflow/runtime
  behavior requires full workspace tests with an isolated PostgreSQL database.
- Security-sensitive changes require an independent exact-head security review.
- Merge only an exact head whose required CI result is successful. Use the
  owner PR-only bypass solely for the author-self-review limitation.
- Do not close #1785 merely because #1814 fixes the review-spawn subset.

## Unknowns and open questions

- Whether the current #1833 replacement guard passes both positive and
  intentional-violation power tests.
- Whether #1814's lifecycle cleanup is complete for timeout, cancellation,
  interrupt, reset, stdin failure, and drop across both persistent adapters.
- Whether #1834 has any remaining correctness issue after #1836 is integrated.
- Which lower-priority architecture issues are already partially satisfied by
  merged work and which require new implementation PRs.
- Whether external review-bot quota recovers; independent exact-head review is
  required regardless.

## Likely codebase touchpoints

- `crates/harness-agents/` and `crates/harness-cli/` for #1814 and #1833.
- `crates/harness-core/src/config*`, `crates/harness-workflow/src/runtime/`,
  and `crates/harness-server/src/` for #1836, #1834, and #1818-#1820.
- `README.md` and onboarding scripts for #1821/#1837.
- `crates/harness-core/src/stack/inventory*` for #1731 remediation.
- Runtime prompt provenance and runtime-profile resolution for #1732
  remediation.
- Repository hygiene, workflow configuration naming, and `specs/` lifecycle
  indexing for #1805.

## Active lane map

- `/root`: integration owner, GitHub operations, dependency ordering, final
  verification, and backlog audit.
- `/root/review_pr1833_final_clean`: isolated #1833 guard redesign; no #1814
  files.
- `/root/review_pr1836_exact`: isolated #1814 security repair; no manifests or
  high-context files.
- `/root/review_pr1834_exact`: read-only exact-head review of #1834.

## Stop conditions

- Exact PR head differs from the reviewed or tested SHA.
- A writer touches files outside its assignment.
- Required database, credential, or external service is unavailable and no
  equivalent repository-local verification exists.
- A security review finds a problem outside the authorized repair scope.
- The same repair hypothesis fails three consecutive times.

## Current execution order

1. Finish, independently review, publish, and merge #1833.
2. Integrate and merge #1836.
3. Integrate, deeply verify, and merge #1834.
4. Integrate and merge #1821.
5. Integrate, security-review, and merge the corrected #1814.
6. Preserve remaining #1835 work through #1818-#1820, then close #1835.
7. Implement #1731/#1732 remediation and close #1812/#1827.
8. Continue through the remaining issues in dependency and severity order.
9. Run final protected-branch and open-backlog audit.
