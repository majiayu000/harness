# Issue 899 Workspace Lifecycle Signal Report

Generated: 2026-04-23

## Signal

PR #919 already implements durable workspace ownership records, startup reconciliation, failure-kind observability, and retry cleanup for issue #899. The latest unresolved review comments identify two remaining lifecycle holes in `crates/harness-server/src/workspace.rs`.

## Root Cause

1. `cleanup_workspace_path` returns immediately when the checkout directory is missing. Git can still retain a registered worktree entry after out-of-band directory deletion, so skipping `git worktree remove --force` and `git worktree prune` leaves repo metadata stale.
2. `reconcile_startup` propagates the first per-workspace cleanup error. A single unreadable or invalid stale workspace can abort the whole startup sweep, leaving later stale workspaces unreconciled.

## Required Fix

- Make cleanup attempt registration removal and pruning even when the checkout path is absent.
- Make startup reconciliation log per-entry cleanup failures and continue scanning the workspace root.
- Add deterministic tests for missing-directory registrations and per-entry reconciliation continuation.
