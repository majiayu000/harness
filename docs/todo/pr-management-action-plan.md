# PR Management Action Plan

## Strong Consensus: Do First

### 1. Stop changing `Cargo.toml` versions in feature PRs

Evidence: tokio, deno, tauri, rust-lang, and OpenAI codex keep feature PRs separate from release version bumps.

Actions:

- Remove any rule that requires every PR to update package versions.
- Bump versions only during release preparation.

Expected impact: removes most recurring `Cargo.toml` merge conflicts.

Rollback: restore the previous version-bump rule.

### 2. Use squash-only merges

Evidence: high-throughput Rust projects commonly use squash merges to reduce merge noise and conflict surface.

Actions:

- Disable merge commits and rebase merges in repository settings.
- Keep one final commit per PR.

Expected impact: each PR maps to one commit and the history is easier to bisect.

Rollback: re-enable other merge strategies.

### 3. Apply model tiering

Evidence: the codebase already has reasoning-budget infrastructure. Simple rebase, format, and version tasks can use cheaper models while implementation and complex review keep stronger models.

Actions:

- Configure low-cost models for simple maintenance tasks.
- Keep stronger models for implementation and complex review.
- Update task-executor model selection accordingly.

Expected impact: lower token cost without reducing quality for complex tasks.

### 4. Reduce review rounds

Evidence: shorter agent-to-agent review loops catch most useful feedback while avoiding runaway cycles.

Actions:

- Set a global maximum turn count.
- Reduce medium-complexity review rounds.
- Cap simple maintenance tasks at very few turns.

Expected impact: lower review cost and faster completion.

## Partial Consensus: Consider Next

### 5. Skip unrelated CI jobs

Add changed-file detection in CI so independent crate tests can be skipped when not affected.

### 6. Auto-close stale PRs

Add stale PR cleanup only after confirming it will not close active work.

### 7. Dynamic concurrency throttling

Extend rate-limit checks with token-usage data and reduce concurrency when budget is low.

### 8. Integrate release-plz

Use release-plz to generate release PRs and changelog updates once the release cadence is decided.

## Needs More Evidence

### 9. Merge queue

Current PR volume does not justify a paid merge-queue dependency. Revisit only if squash-only plus faster CI still leaves frequent merge conflicts.

## Explicit Non-Goals

- Do not introduce a paid merge-queue service now.
- Do not add a custom bot for version bumps unless release-plz cannot satisfy the workflow.

## Execution Order

- Day 1: remove per-PR version-bump rules, enable squash-only, lower review rounds.
- Day 2: add model tiering for simple maintenance tasks.
- Week 1: measure conflict rate and CI time.
- Week 2: decide whether changed-file CI and release-plz are worth adding.

## Data Sources

| Source | Type | Notes |
|---|---|---|
| Agent A | Internal | OpenAI codex PR workflow comparison |
| Agent B | Internal | Rust monorepo version strategy comparison |
| Agent C | Internal | Reasoning budget and rate-limit analysis |
| Grok | External | Release and model-tiering recommendations |
| Gemini | External | Unavailable during this research pass |
