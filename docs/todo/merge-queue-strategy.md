# Merge Queue Strategy

## Problem

18 PRs open simultaneously cause cascading merge conflicts. Each merge changes `main`, invalidating all other PRs' rebase state. Manual rebase-and-merge cycles are expensive (Sonnet API quota, time).

## Root Causes

1. **Parallel PR explosion**: Harness generates PRs faster than they can be merged
2. **Cargo.toml version bump in every PR**: Most frequent conflict source — each PR bumps the workspace version, guaranteed conflict with any other PR
3. **No merge ordering**: PRs are merged in arbitrary order, maximizing conflict surface

## Solutions

### S1: Enable GitHub Merge Queue (recommended)

GitHub native feature. PRs enter a queue, each is automatically rebased onto the latest `main` and CI-validated before merge.

```
Settings → Branches → Branch protection rules → Require merge queue
```

Benefits:
- Zero manual rebase work
- CI runs against actual merge result
- Prevents broken `main`

### S2: Remove per-PR version bumps

Current: every PR bumps `Cargo.toml` version → guaranteed conflict.

Proposed: only bump version at release time or via a dedicated `chore: bump version` commit. Feature PRs should NOT modify the workspace version field.

Update CLAUDE.md rule:
```
## Version Bumps
- Do NOT bump Cargo.toml workspace version in feature/fix PRs
- Version bumps happen only at release time via dedicated commit
```

### S3: Harness serial merge strategy

Add a `merge_queue` phase to the task pipeline:
1. Pick highest-priority mergeable PR
2. Rebase onto latest `main`
3. Run CI
4. Merge
5. Repeat

This prevents the "rebase all 18 at once, merge one, 17 conflict again" cycle.

## Conflicting PRs (as of 2026-04-12)

These 5 PRs have deep code conflicts and need manual resolution or recreation:

| PR | Conflict Files | Recommendation |
|----|---------------|----------------|
| #615 | `periodic_reviewer.rs` | Close — #614 (merged) covers same issue |
| #650 | CI failing (Security Audit, Test) | Fix CI or close |
| #653 | `http.rs`, `task_executor.rs`, `task_runner.rs` (file delete/modify) | Close — large refactor needs fresh approach |
| #655 | `http.rs`, `task_db.rs`, `task_runner.rs` | Recreate from latest main |
| #659 | `event_replay.rs` | Close — #661 (merged) covers same area |
