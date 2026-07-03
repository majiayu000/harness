# Agent Run Identity Task Plan

Spec: `specs/session-identity/` (PR #1425)

## Thread Lane Map

- `SI-L1`: implementation worker. Owns `harness-core` run-id module, `harness-agents` spawn paths, `harness-observe` stamping, and their tests.
- `SI-L2`: reviewer, read-only. Owns post-implementation diff review against this spec.

No two writable lanes may edit the same file. `AGENTS.md`, `.claude/*`, hooks, settings, and global config are forbidden files unless explicitly requested.

## Tasks

### SI-T1: Run-id primitives in harness-core

Owner: SI-L1

Dependencies: spec merged

Done when:

- `RunId` type with `ar-<ulid>` (lowercase) formatting, parsing, and validation.
- Mint-only-if-absent env resolution: reads `AGENT_RUN_ID`, honors existing value, nested-mint moves inherited value to `AGENT_RUN_PARENT`.

Verify:

```sh
cargo test -p harness-core run_id
```

### SI-T2: Binding registry module

Owner: SI-L1

Dependencies: SI-T1

Done when:

- Append-only JSONL writer (`O_APPEND`, single line < 4 KB) at `${XDG_STATE_HOME:-~/.local/state}/agent-run/bindings.jsonl`.
- Corruption-tolerant reader (skips malformed lines), latest-wins on duplicate `(run_id, native.id)`.
- 10 MB size rotation keeping one rotated file.
- Registry write failure logs at error level and never blocks execution.

Verify:

```sh
cargo test -p harness-core registry
```

### SI-T3: Adapter env injection and provisional bindings

Owner: SI-L1

Dependencies: SI-T1, SI-T2

Done when:

- Claude Code and Codex spawn paths in `harness-agents` export `AGENT_RUN_ID` (minting when absent).
- A provisional binding line (empty `native.id`, `source: harness-adapter`) is written at spawn.
- Env survives the adapter's Claude-prefix sanitization (regression test).

Verify:

```sh
cargo test -p harness-agents run_id
```

### SI-T4: Event stamping in harness-observe

Owner: SI-L1

Dependencies: SI-T3

Done when:

- Task/thread/turn events carry a nullable `run_id`; `event/query` exposes it.
- Bare-terminal runs (no run id) produce unchanged behavior and no errors.

Verify:

```sh
cargo test -p harness-observe run_id
```

### SI-T5: Reference hook snippet and docs

Owner: SI-L1

Dependencies: SI-T2

Done when:

- `docs/run-identity.md` documents the env contract and registry format, with a copy-paste Claude Code `SessionStart` hook snippet that writes the authoritative binding.

Verify:

```sh
test -f docs/run-identity.md
```

### SI-T6: Cross-repo adoption issues

Owner: coordinator

Dependencies: SI-T5

Done when:

- One tracking issue each in keepline (join + `ps -Eww` fallback; recovery inherits run id with `recovered_from`), ccstats (`run_id` column at ingest), remem (provenance field from env).

Verify:

```sh
gh issue list -R majiayu000/keepline --search "run identity" | grep -q .
```
