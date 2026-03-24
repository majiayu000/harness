# Sprint Plan — 2026-03-24

Issues: #528, #529, #530, #532

## Dependency Analysis

| Issue | Title | Priority | Key Files |
|-------|-------|----------|-----------|
| #528 | Event sourcing formalization | P0 | task_executor.rs, task_runner.rs, task_db.rs, event_replay.rs (new) |
| #532 | Checkpoint-based crash recovery | P0 | task_db.rs, task_executor.rs, task_runner.rs |
| #529 | Triage complexity output | P1 | prompts.rs, task_executor.rs |
| #530 | Impasse detection | P2 | task_executor.rs (review loop only) |

## Rationale

- **#528 first**: foundational infrastructure; all other issues that touch `task_executor.rs` or `task_runner.rs` must wait for its structural changes to land.
- **#532 after #528**: explicitly states dependency on #528 (event replay needed before checkpoints). Touches the same three files — running concurrently would cause merge conflicts.
- **#529 and #530 after #532**: both edit `task_executor.rs` which #528 and #532 substantially restructure. Waiting for both P0s avoids conflicts. #529 and #530 touch different sections (triage path vs review loop) so they can run in parallel.

## Sprint Plan

SPRINT_PLAN_START
{
  "tasks": [
    {"issue": 528, "depends_on": []},
    {"issue": 532, "depends_on": [528]},
    {"issue": 529, "depends_on": [532]},
    {"issue": 530, "depends_on": [532]}
  ],
  "skip": []
}
SPRINT_PLAN_END
