# Autonomous merge completion — 2026-09-10

The operator authorized Cursor to continue repairing managed PRs and merge them
when repository conditions are satisfied. Production configuration now enables
automatic squash merge through the agent, with clean mergeability, resolved
review threads, and server verification of merge completion. Remote comment
polling remains disabled.

Changes:

- Automatic merge polling no longer depends on the remote comment polling flag.
- Before automatic merge, Harness schedules an independent local review tied
  to a server-observed head SHA. A changed head requires another review.
- BEHIND/DIRTY mergeability or failing check rollups cause a new review rather
  than silently leaving a ready workflow idle. Review instructions explicitly
  route actionable mergeability/CI findings to the existing Cursor repair loop.
- A matching local review can satisfy the review part of merge readiness;
  successful GitHub check rollup, mergeability, and thread conditions remain.
  GitHub branch protection remains authoritative at merge time.
- Retryable/external-dependency merge failures return to independent local
  review. Explicit operator recovery can also re-review an already stopped
  merge activity, which was used once for Harness #2048.
- The existing dedupe validator method moved into the existing command-rules
  module to keep the validator below its enforced file limit; behavior is unchanged.

Verification:

- `cargo check -p harness-workflow --all-targets`
- `cargo check -p harness-server --all-targets`
- `cargo build -p harness-cli --bin harness`
- `cargo fmt --all -- --check`
- Real production execution: Harness automatically scheduled review and merge
  for `majiayu000/loom#679`. Cursor squash-merged head
  `955114e210f56d49e92e27fde1090144b5acf43e`; GitHub independently confirmed merge
  commit `14e3dfa2d64799c70c5ea1bd2ebb0167925ea24d` at
  `2026-09-10T15:46:00Z`. The parent workflow reached `done`.
- The existing reconciliation API synchronized externally completed work,
  including Harness #2049. This was a one-time operator reconciliation; the
  periodic reconciliation configuration was not enabled in this deployment.

No simulated Agent output was used as verification. At observation time there
were 13 merging, four repairing, and eight reviewing workflows; those counts
are not completion claims. Missing checks, unresolved threads, and other real
blockers can still stop a workflow. Historical child failures remain in the
audit trail. Changes are deployed but uncommitted.

Evidence:
`/Users/apple/.local/share/harness/cursor-batches/20260908-233757/merge-loop-20260910/`

Production binary: `../binary/harness-merge-loop`, API port 19800; separately
served dashboard port 19801. The debug binary still embeds the stub frontend
because Bun was unavailable during the build.
