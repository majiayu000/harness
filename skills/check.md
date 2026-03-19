# Use when: running all guard scripts to produce a project health report, before committing, or after significant changes. Triggers on: health report, project check, run guards, guard scripts, project health

<!-- trigger-patterns: health report, project check, run guards, guard scripts, project health -->

## Trigger
Use to run all guard scripts and produce a project health report. Run before committing or after significant changes.

## Input
Project root path. Optionally a list of recently changed files.

## Procedure
1. Discover all guard scripts in `.harness/guards/`, `~/.harness/guards/`, and `/etc/harness/guards/`.
2. Execute each guard script in order, capturing stdout, stderr, and exit code.
3. Classify each result: exit 0 = pass, exit 1 = warn, exit 2 = block.
4. Aggregate counts: total pass, warn, block.
5. For each warn/block, extract the file and line reference if available.
6. Compute overall health grade: A (0 blocks, 0 warns), B (0 blocks, ≤3 warns), C (0 blocks, >3 warns), D (any block).
7. Output recommendations for each block and warn.

## Output Format
```
# Health Report

Grade: <A|B|C|D>
Pass: <N> | Warn: <N> | Block: <N>

## Blocks
- [<guard>] <file>:<line> — <message>

## Warnings
- [<guard>] <file>:<line> — <message>

## Recommendations
- <action to resolve block/warn>

## Passed Guards
- <guard name>: ok
```

## Constraints
- A "block" finding must be resolved before committing.
- Do not suppress guard output — report all findings regardless of severity.
- If no guards are found, report "no guards configured" rather than a passing grade.
