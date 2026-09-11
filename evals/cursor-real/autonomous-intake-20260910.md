# Autonomous GitHub intake check — 2026-09-10

## Scope and result

Configured Harness to poll the existing private repository
`majiayu000/harness-cursor-eval-20260910-152946` every 30 seconds, using
Cursor and the existing isolated workflow database. No runtime submission API
calls or manual implementation/review dependency pairs were made in this run.

Harness discovered PR #1, scheduled `run_local_review`, and advanced it to
`ready_to_merge` after a real Cursor review. The review reported passing
typecheck, tests, and coverage. Its transcript is 198,330 bytes, with checksum
`sha256:20819c8d9c2f7fdfdce3fd53fdb54d85782cc65c208b3cf0aef03c2d5fced5b5`.
Cursor thread: `9b6f55e8-279e-4661-ac3f-5a23fbe3b653`.

PR #1 already contained the repair from an earlier run. This establishes
autonomous discovery and review completion, not a new autonomous repair or
Issue planning result. Neither PR was merged.

## Defect found and fixed

An already-reviewed PR (#2) was repeatedly rediscovered, but the execution
service rejected it because `ready_to_merge` is ineligible for a feedback sweep.
GitHub polling now recognizes that state as an existing submission and returns
its task identity. Explicit requests from other sources keep their existing
behavior. No workflow states or evaluation scores were rewritten.

Verification: `cargo check -p harness-server --all-targets`,
`cargo build -p harness-cli --bin harness`, and `cargo fmt --all -- --check`
passed. After restarting the isolated server with the fix, a 65-second
observation covering two polling intervals found no intake enqueue errors and
no additional jobs (four total succeeded jobs, including the three earlier
native pilot jobs). No simulated Agent output tests were used.

The initial background launcher did not survive its invoking tool session;
the actual successful check used the sanitized foreground launcher.
The debug build embedded a stub UI because Bun was unavailable to the build;
this run verifies backend behavior only.

## Evidence and remaining scope

Evidence directory:
`/Users/apple/.local/share/harness/cursor-evals/20260910-170001-native-fixed/evidence/`

- `autonomous-intake-config.json`
- `autonomous-runtime_jobs.json`
- `autonomous-workflow_instances.json`
- `autonomous-dedup-verification.json`

The server remains configured on port 19820, with automatic merge disabled.
The fix is uncommitted and deployed only to this isolated evaluation server.

The separate 14-case batch was manually assembled as implementation/review
pairs. It is execution evidence only and must not count as autonomous workflow
coverage. Its existing jobs were neither duplicated nor manually repaired here.
All-case autonomous evaluation remains incomplete. Production stopped workflows
were inspected, not reset: many retain the earlier missing-validation-command
configuration failure, and ordinary transient auto-recovery cannot resolve that
historical state by merely being enabled.
