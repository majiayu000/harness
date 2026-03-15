---
source: remem.save_memory
saved_at: 2026-03-14T15:37:56.154509+00:00
project: harness
---

# Codex CLI 0.114.0 Breaking API Change — Flag and Value Migration

## Codex CLI 0.114.0 Breaking Change (discovered 2026-03-14)

### What changed
Codex CLI removed the `-a` (approval mode) flag and replaced it with `-s` (sandbox mode). The accepted values also changed:

| Harness SandboxMode | Old flag (`-a`) | New flag (`-s`) |
|---------------------|----------------|----------------|
| ReadOnly | `-a read-only` | `-s read-only` |
| WorkspaceWrite | `-a read-write` | `-s workspace-write` |
| DangerFullAccess | `-a full-access` | `-s danger-full-access` |

### Affected file
`crates/harness-agents/src/codex.rs` — `base_args()` method and `codex_sandbox_mode()` function.

### Root cause of recurrence
This bug was first fixed on 2026-03-06 (commit `247f735`) but that fix **never merged to main**. It only existed on 3 stale feature branches (`fix/signal-thresholds-from-config`, `fix/thread-manager-dead-code`, `fix/unify-violation-logging`). Subsequent major refactors of codex.rs (sandbox isolation, cloud setup, streaming) all branched from main's buggy version, so the fix was lost.

### Lesson — Branch Fix Loss Pattern
When an autonomous agent fixes a bug on a feature branch but the PR is not merged:
1. The fix exists only on that branch
2. Main continues with the bug
3. Any refactor branching from main carries the bug forward
4. The fix becomes unreachable as the branch goes stale

### Prevention
- Critical bug fixes (especially external CLI compatibility) should be merged to main IMMEDIATELY, not bundled with feature PRs
- After Codex review failures, check if the failure is a harness-side bug (not a task-side bug) before retrying
- When upgrading external CLI dependencies (codex, claude), run `codex exec --help` / `claude --help` to verify flag compatibility
- Add a startup self-test: on server boot, run `codex exec --help` and verify expected flags exist
