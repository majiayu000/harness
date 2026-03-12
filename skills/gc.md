# GC

<!-- trigger-patterns: garbage collect, disk usage, clean worktrees, archive logs, code smells -->

## Trigger
Use periodically (weekly or when disk usage is high) to archive old logs, clean worktrees, and scan for code smells.

## Input
Project root path. Optionally a date threshold for log archiving (default: 30 days).

## Procedure
1. **Log archiving**: Find event logs older than threshold in `~/.local/share/harness/events/`. Compress and move to `archive/`. Report bytes freed.
2. **Worktree cleanup**: List all git worktrees (`git worktree list`). For each stale worktree (no associated open PR, no recent commits), prompt for removal confirmation.
3. **Code smell scan**: Run static analysis for:
   - Dead code (functions defined but never called).
   - TODO/FIXME/HACK comments older than 30 days.
   - Files exceeding size limits (>800 lines).
   - Duplicate logic blocks (>10 lines repeated verbatim).
4. **Dependency audit**: Run `cargo audit` / `npm audit` / `pip audit` as appropriate. Report critical CVEs.
5. **Temp file cleanup**: Remove `.tmp`, `.bak`, and editor swap files from the project tree.
6. Report total disk space recovered and remaining smells requiring human attention.

## Output Format
```
# GC Report

Date: <timestamp>
Space freed: <N> MB

## Log Archive
- Archived <N> log files (<size>) to archive/

## Worktrees
- Cleaned: <list>
- Kept (active): <list>

## Code Smells
- <file>:<line> — <smell type>: <description>

## Dependency Audit
- <package>@<version> — CVE-<ID> (<severity>)

## Temp Files Removed
- <N> files, <size>
```

## Constraints
- Never delete files without reporting what will be deleted first.
- Worktree removal requires explicit confirmation — do not auto-delete.
- Do not archive logs from the current day.
- Report CVEs even if no upgrade path exists.
