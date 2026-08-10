# Harness — Claude Code Rules

Read and follow `AGENTS.md`; it is the canonical source for shared project rules. This file contains only Claude Code-specific additions. If duplicated guidance is discovered, remove it from this file rather than maintaining two copies.

## Claude CLI Argument Order (CRITICAL)

- Claude CLI `-p` takes its prompt as the NEXT token: `claude -p <PROMPT> [other flags...]`
- The prompt MUST immediately follow `-p`. Placing it at the end of the arg list causes "Input must be provided" errors
- `claude.rs` (CodeAgent) is the only Claude CLI spawn path — the `ClaudeAdapter` turn path was removed as unreachable (GH-1786); the shared stream-json line parsers live in `claude_stream_json.rs`
- After modifying CLI arg construction, verify with: `cargo test --package harness-agents`

## Server Operation

- Nested Claude session markers can make spawned agents fail. When product behavior must be exercised from Claude Code, start the server with `scripts/start-harness-codex-safe.sh` or an equivalent launcher that removes the keys listed in `crates/harness-agents/src/spawn_contract.rs`.
- Prefer a standalone terminal for long-running manual dogfood sessions when an operator needs an independently owned process.
- Before starting a server, check whether the target port is already in use. Stop only Harness processes you started unless the user explicitly asks otherwise.
