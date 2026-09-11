# Local-review completion fix: real before/after — 2026-09-10

Outcome: **the native PR review/repair/re-review path now reaches the existing
merge-decision gate without requiring preconfigured server validation commands**.
This is not a merge or a claim that all repositories are ready to merge.

## Code change

For ordinary workflows, `build_local_review_completed_decision` now moves a passed
local review to `ready_to_merge` with no follow-up command. The existing operator
merge gate remains. Evaluator-owned benchmark workflows with server-provenanced
eval metadata retain their quality gate. The transition validator, state registry,
local-review prompt and Cursor documentation are aligned with this boundary.
No new configuration, agent tool restriction, dependency, or repository-specific
validation command was added. Repair-round limits and remote-feedback routes
were not changed. Existing failed production runs were not rewritten or retried.

## Real comparison

| Observation | Before: PR #1 | After: PR #2 |
|---|---|---|
| Starting candidate | `2abc521f947cfdb059dbe28a42a5347829391e55` | Same commit |
| Initial task | Native PR submission | Native PR submission |
| Manual follow-up dispatch | None | None |
| First local review | Changes requested | Changes requested |
| Cursor repair | Pushed fix | Pushed fix |
| Fresh Cursor re-review | Passed | Passed |
| Extra quality-gate activity | Started and failed | Not created |
| Final workflow state | `failed` | `ready_to_merge` |
| Error | `validation_commands_missing` | None |

After-run PR: [majiayu000/harness-cursor-eval-20260910-152946#2](https://github.com/majiayu000/harness-cursor-eval-20260910-152946/pull/2).
Actual pushed head: `0625df24e35c2675043ad9c4d8a59f9aa83c97bb`.
The three activities used distinct Cursor thread IDs, recorded with raw transcript
references in `evidence/runtime_jobs.json`. No simulated Agent outputs were used.
The evaluator fetched the pushed head into a separate checkout and independently
ran frozen-lockfile install, typecheck, all 193 panel tests, and coverage; all
returned exit 0. No source changes were made by the evaluator in the candidate.

## Validation and deployment

- `cargo fmt --all -- --check`: passed.
- `cargo check -p harness-workflow --all-targets`: passed.
- `cargo check -p harness-server --all-targets`: passed.
- `cargo build -p harness-cli --bin harness`: passed.
- No mocked Agent-output test suite was run. The existing transition assertion
  was updated to call the decision builder directly, covering ordinary and
  evaluator-owned branches without constructing a fake Agent response; it was
  compile-checked, not claimed as an executed test.
- Patch attribution recorded six task-modified existing files before this report;
  no preexisting changes were discarded.
- The idle, previously started production server on port 19800 was safely replaced
  with `harness-local-review-ready`; health and the port-19801 monitor returned 200.

The real trial used the new transition logic and an explicit next-state prompt.
After observing a reviewer still describe the old next stage, the prompt was
simplified to request only the review outcome and leave next-state selection to
Harness. That final wording-only change was built successfully and deployed; it
was not a second three-stage live trial.

## Limitations and retained evidence

This is one development case with model `auto`, not a statistically controlled
quality improvement or a full benchmark score. An external Codex review bot
commented on both PRs; the repair Agent read/responded to that feedback. Both
runs therefore include an external advisor. No GitHub CI checks were reported on
these private evaluation PRs; absent checks are not passing CI. `ready_to_merge`
here is the runtime's waiting-for-merge-decision state, not independent proof of
all repository merge prerequisites. Both PRs remain open and unmerged.

The old native failure is preserved in `native-pr-pilot-20260910.md`. This run's
local evidence directory is `/Users/apple/.local/share/harness/cursor-evals/20260910-170001-native-fixed/evidence`, including actual submitted input,
workflow/job records, remote head/check/review snapshot, independent execution
logs, and the archived final candidate panel. The original business PRs were not
changed by this trial. Production historical failures still need explicit recovery;
this deployment only changes subsequent local-review completion behavior.
