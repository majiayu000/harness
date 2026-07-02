# GH1415 Tasks

Issue: `#1415`
Product spec: `specs/GH1415/product.md`
Tech spec: `specs/GH1415/tech.md`

## Status

- [ ] `SP1415-T001` Owner: `config` | Done when: `merge_execution` ("agent"/"server", default "agent") and `verify_merge_completion` (default true) exist in intake/auto-merge config, are logged at startup, and are documented | Verify: `cargo test --package harness-server config`
- [ ] `SP1415-T002` Owner: `runtime-worker` | Done when: merge_pr activity completions that report success are accepted only after a server-side `fetch_github_pr_snapshot` read confirms the PR is merged, and mismatches route through the invalid-agent-output failure path | Verify: `cargo test --package harness-server workflow_runtime_worker`
- [ ] `SP1415-T003` Owner: `runtime-worker` | Done when: verification evidence (merged flag, PR state, snapshot timestamp, head SHA) is persisted into the activity-result artifacts and visible in decision records | Verify: `cargo test --package harness-server workflow_runtime_worker`
- [ ] `SP1415-T004` Owner: `github-client` | Done when: `github_pr_merge.rs` implements the REST merge call reusing `resolve_github_token` and maps 200/405/409/422 and already-merged to the existing retryable/non-retryable error classes | Verify: `cargo test --package harness-server github_pr_merge`
- [ ] `SP1415-T005` Owner: `dispatcher` | Done when: with `merge_execution = "server"`, merge_pr commands route to a builtin lifecycle activity (gate freshness re-check, merge call, confirmation read) instead of an agent prompt packet | Verify: `cargo test --package harness-server`
- [ ] `SP1415-T006` Owner: `tests` | Done when: a simulated false `merged=true` agent report produces a blocked/failed decision and never a terminal success state, in both execution modes | Verify: `cargo test --package harness-server`
- [ ] `SP1415-T007` Owner: `docs` | Done when: token permission requirements for server-executed merges and the rollout order (verify-first, then server mode) are documented | Verify: `test -s docs/usage-guide.md`

## Parallelization

- Lane A (T002, T003, T006): workflow runtime worker completion path.
- Lane B (T004, T005): new merge client and dispatcher routing.
- T001 lands first as a small standalone change so both lanes stay disjoint on files.

## Verification

- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
- `cargo test --package harness-server`
- Manual auto-merge run in both modes on a test repository, evidence captured in the implementation PR body.

## Handoff Notes

- Phase 1 (T001-T003, T006) is independently shippable; do not block it on Phase 2 (T004, T005).
- No `gh`/`git` subprocesses in harness crates; HTTPS API only.
- Reuse the reducer's blocked-decision path for invalid results; do not invent a new failure channel.
