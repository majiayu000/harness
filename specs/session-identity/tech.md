# Agent Run Identity Tech Spec

## Context

harness spawns agent CLIs through `harness-agents` adapters; keepline discovers sessions by scanning processes and parsing CLI JSONL files; remem and vibeguard ride inside the CLIs via hooks; ccstats parses usage after the fact. Each tool keys its data by whatever identity it can see locally. This spec defines the shared key and how it travels.

## Design

### ID format

```
ar-<ULID, lowercase>          e.g. ar-01j1qb3c9r7v5m2k8x4tznq6wd
```

ULID gives sortable, collision-safe, timestamp-embedded IDs with no coordination. The `ar-` prefix makes the value greppable and self-describing in logs.

### Environment contract

| Variable | Meaning |
|---|---|
| `AGENT_RUN_ID` | The run id for this agent process and its children |
| `AGENT_RUN_PARENT` | Set by a nested minter to its inherited `AGENT_RUN_ID` before minting its own |

Rules:

1. **Mint only if absent.** If `AGENT_RUN_ID` is already set, do not overwrite it — the outermost owner wins. A tool that intentionally starts a *nested* run (agent spawning agent) moves the inherited value to `AGENT_RUN_PARENT` and mints a fresh `AGENT_RUN_ID`.
2. The names deliberately avoid the `CLAUDE` prefix: harness strips Claude-prefixed variables before spawning child agents (per current adapter behavior), and this contract must survive that sanitization.
3. Minters in MVP: `harness-agents` adapters at spawn time. Later: keepline recovery, an optional shell wrapper for manual terminals.

### Binding registry

Path: `${XDG_STATE_HOME:-~/.local/state}/agent-run/bindings.jsonl`, append-only JSONL, one binding per line:

```json
{"v":1,"run_id":"ar-01j1...","parent":null,"native":{"kind":"claude-code","id":"<session-uuid>"},
 "pid":12345,"cwd":"/path/to/project","started_at":"2026-07-03T10:00:00+08:00","source":"claude-hook"}
```

- `native.kind`: `claude-code` | `codex` | other adapters later.
- `source`: which component wrote the line (`claude-hook`, `codex-hook`, `harness-adapter`).
- Writers open with `O_APPEND` and write a single line < 4 KB per record; readers must skip lines that fail to parse (corruption-tolerant, never fatal).
- Duplicate bindings for the same `(run_id, native.id)` are allowed; readers take the latest by `started_at`.
- Rotation: when the file exceeds 10 MB, the writer renames it to `bindings.1.jsonl` and starts fresh; readers consult at most the current + one rotated file.

Who writes the binding: the CLI-side hook is the authoritative writer because only it reliably knows the native session id at start time (Claude Code `SessionStart` hooks receive the session id and inherit the environment). The harness adapter additionally writes a provisional line with `native.id` empty at spawn, so a crashed-before-hook run is still traceable.

### Adoption per tool (follow-up scope)

| Tool | Change |
|---|---|
| harness-observe | Stamp `run_id` on task/thread/turn events; expose in `event/query` |
| keepline | Join scanner results to bindings by native session id; fallback `ps -Eww <pid>` (same-user) to read `AGENT_RUN_ID` when no binding exists |
| ccstats | Add a `run_id` column resolved via bindings at ingest time |
| remem | Read `AGENT_RUN_ID` from env inside its hook; store as provenance field when present |

Each adoption is one small, independent PR in its own repo; none depends on another.

## Data Model

No shared database. Each tool adds one nullable `run_id` column/field to its own store. The registry file is the only shared artifact and it is reconstructible (worst case: lose joins for past runs, never lose tool-local data).

## Privacy and Failure Behavior

- The registry contains paths and pids but no prompt/response content.
- Missing env var → no binding → all tools behave exactly as today. Absence is a supported state, not an error.
- Registry unwritable (permissions, disk full): writers log at error level and continue the run — identity is an observability feature and must never block agent execution.

## Verification Plan

- Unit: mint-only-if-absent precedence; nested-run parent handoff; reader skips malformed lines.
- Integration: `harness exec` spawns a real Claude Code run → assert child env contains `AGENT_RUN_ID`, registry gains a binding line with the CLI's session UUID, and harness events carry the same run id.
- Negative: run Claude Code from a bare terminal → assert no binding written, keepline/ccstats output unchanged from baseline.

## Rollback

Remove env injection from the adapters; the registry file becomes inert data. No schema migrations to unwind (all adopted columns are nullable).
