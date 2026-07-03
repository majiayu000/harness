# Agent Run Identity

Harness assigns one local run id to each agent process it starts so observability tools can join Harness events, native CLI sessions, cost rows, and memory provenance without a daemon or network service.

## Environment Contract

Harness exports these variables before spawning Claude Code or Codex:

| Variable | Meaning |
| --- | --- |
| `AGENT_RUN_ID` | The run id for this agent process and its children. |
| `AGENT_RUN_PARENT` | The inherited parent run id when a tool intentionally starts a nested run. |

Run ids use this format:

```text
ar-<lowercase-ulid>
```

Example:

```text
ar-01j1qb3c9r7v5m2k8x4tznq6wd
```

Minters must only mint when `AGENT_RUN_ID` is absent. If a nested minter intentionally starts a new run, it moves the inherited `AGENT_RUN_ID` value to `AGENT_RUN_PARENT` and exports a freshly minted `AGENT_RUN_ID`.

The variable names intentionally avoid the `CLAUDE` prefix so they survive Harness adapter sanitization for nested Claude Code detection.

## Binding Registry

Bindings are append-only JSONL records at:

```text
${XDG_STATE_HOME:-~/.local/state}/agent-run/bindings.jsonl
```

Each line maps a Harness run id to a native CLI session identity:

```json
{"v":1,"run_id":"ar-01j1qb3c9r7v5m2k8x4tznq6wd","parent":null,"native":{"kind":"claude-code","id":"<session-uuid>"},"pid":12345,"cwd":"/path/to/project","started_at":"2026-07-03T10:00:00Z","source":"claude-hook"}
```

Fields:

| Field | Meaning |
| --- | --- |
| `v` | Registry record version. Current value is `1`. |
| `run_id` | Harness agent run id. |
| `parent` | Parent run id for nested runs, otherwise `null`. |
| `native.kind` | Native adapter or CLI kind, such as `claude-code` or `codex`. |
| `native.id` | Native session id. Harness adapter provisional records use an empty string until a CLI-side hook writes the authoritative binding. |
| `pid` | Process id of the spawned CLI process. |
| `cwd` | Working directory for the agent process. |
| `started_at` | RFC 3339 timestamp. |
| `source` | Writer name, such as `harness-adapter`, `claude-hook`, or `codex-hook`. |

Writers open the file with append mode and write one JSON line per record. Readers skip malformed lines and take the latest record for each `(run_id, native.id)` pair by `started_at`.

When `bindings.jsonl` reaches 10 MB, the writer rotates it to `bindings.1.jsonl` and starts a new current file. Readers consult the current file and one rotated file.

Registry write failures are logged as errors and must not block agent execution.

## Claude Code SessionStart Hook

Claude Code `SessionStart` hooks inherit `AGENT_RUN_ID`, so they can write the authoritative binding with the native `session_id`.

Reference hook:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "python3 - <<'PY'\nimport json, os, pathlib, sys\nfrom datetime import datetime, timezone\n\nrun_id = os.environ.get('AGENT_RUN_ID')\nif not run_id:\n    sys.exit(0)\n\ntry:\n    payload = json.load(sys.stdin)\n    session_id = payload.get('session_id') or payload.get('sessionId') or ''\n    pid = payload.get('pid') or payload.get('process_id') or os.getppid()\n    state_home = pathlib.Path(os.environ.get('XDG_STATE_HOME') or pathlib.Path.home() / '.local' / 'state')\n    path = state_home / 'agent-run' / 'bindings.jsonl'\n    path.parent.mkdir(parents=True, exist_ok=True)\n    record = {\n        'v': 1,\n        'run_id': run_id,\n        'parent': os.environ.get('AGENT_RUN_PARENT'),\n        'native': {'kind': 'claude-code', 'id': session_id},\n        'pid': pid,\n        'cwd': os.getcwd(),\n        'started_at': datetime.now(timezone.utc).isoformat().replace('+00:00', 'Z'),\n        'source': 'claude-hook',\n    }\n    with path.open('a', encoding='utf-8') as fh:\n        fh.write(json.dumps(record, separators=(',', ':')) + '\\n')\nexcept Exception as exc:\n    print(f'agent run binding hook failed: {exc}', file=sys.stderr)\nPY"
          }
        ]
      }
    ]
  }
}
```

Bare terminal runs that do not have `AGENT_RUN_ID` skip the hook body and keep current behavior.
