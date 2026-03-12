# Stats

<!-- trigger-patterns: hook stats, hook statistics, compliance trends, violated rules, aggregated stats -->

## Trigger
Use to view aggregated hook statistics, compliance trends, and identify the most violated rules.

## Input
Project root path. Optionally a time window (default: last 30 days).

## Procedure
1. Read event logs from `~/.local/share/harness/events/` for the specified time window.
2. Filter events of type: interceptor decision (pass/warn/block), signal detection, GC trigger.
3. Aggregate by:
   - Total turns processed.
   - Interceptor decisions: pass count, warn count, block count, block rate %.
   - Top 5 rules by violation frequency.
   - Signal types triggered: RepeatedWarn, ChronicBlock, HotFiles, SlowSessions, etc.
   - GC cycles triggered vs scheduled.
4. Compute compliance trend: compare this period vs previous period (delta).
5. Identify the single highest-impact improvement: the one rule fix that would reduce block rate the most.

## Output Format
```
# Stats Report

Period: <start> — <end>
Total turns: <N>

## Interceptor Decisions
Pass: <N> (<pct>%) | Warn: <N> (<pct>%) | Block: <N> (<pct>%)
Block rate: <pct>% (prev period: <pct>%, delta: <+/->pct>%)

## Top Violated Rules
1. <rule ID>: <N> violations — <description>
2. <rule ID>: <N> violations
3. <rule ID>: <N> violations
4. <rule ID>: <N> violations
5. <rule ID>: <N> violations

## Signal Detections
- <signal type>: <N> occurrences

## GC Activity
Scheduled: <N> | Triggered: <N> | Ratio: <pct>%

## Highest-Impact Improvement
Fix "<rule ID>" to reduce block rate by ~<pct>%
```

## Constraints
- Report actual counts only — do not estimate or extrapolate.
- If insufficient data exists for trend comparison, report "insufficient history".
- Do not include PII or sensitive content from task prompts in the report.
