# Production Cursor workflow recovery — 2026-09-10

Deployed `harness-production-recovery` to the existing production server on
port 19800. Health returned `ok`; the separate dashboard on port 19801 returned
HTTP 200. Existing GitHub intake covers 87 configured repositories with 20
concurrent runtime slots. Automatic merge remains disabled.

The build includes the GitHub intake deduplication fix and an explicit operator
recovery path for ordinary PR workflows stopped by the missing-validation-command
gate. Calling the existing unblock/retry endpoint with
`target_state: local_review_gate` now schedules a fresh independent review for
that condition. It does not replay the obsolete child gate or mark the PR passed.
Evaluator-owned workflows and other stop reasons are excluded from this path.

Read-only inventory found 41 stopped parent workflows matching that condition.
GitHub checks found 35 still-open PRs; six closed or unverifiable subjects were
excluded. All 35 recovery requests were accepted through the existing runtime
API. No SQL updates or replacement implementation/review submissions were used.
An initial recovered task actually spawned Cursor before the remaining recovery
requests were sent.

After recovery, the runtime reported 20 running and 15 pending local-review
activities. These are execution states, not passing evaluation results. Subsequent
review, repair, and independent re-review are owned by Harness. Maintainer-frozen
tasks and unrelated failures were not unblocked.

Verification: `cargo check -p harness-workflow --all-targets`,
`cargo build -p harness-cli --bin harness`, `cargo fmt --all -- --check`, and the
real production recovery requests above. No simulated Agent-output tests were
used. Changes remain uncommitted. The debug build embedded a stub UI due to Bun
not being available in the build environment; the separately served dashboard
was checked independently.

Evidence directory:
`/Users/apple/.local/share/harness/cursor-batches/20260908-233757/production-recovery-20260910/`

Contains pre-recovery workflow records, GitHub subject checks, every recovery
response, post-recovery active jobs, and deployment metadata. Historical failed
child workflows remain as evidence; they were not rewritten as successful.
