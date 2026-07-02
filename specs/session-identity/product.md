# Agent Run Identity Product Spec

## Summary

One correlation ID (`run id`) for a single agent run, minted by whoever starts the run and propagated to every tool that observes it. harness tasks, keepline sessions, remem memories, and ccstats cost rows about the same run become joinable instead of living under four unrelated identities.

## User Problem

Today the same agent run has a different identity in every tool: a harness task/thread ID, a keepline session ID derived from the CLI's JSONL file, remem provenance keyed its own way, ccstats attribution keyed by native session. Nobody can answer basic operational questions across tools: "what did this harness task cost", "which memories came out of this run", "show me the keepline timeline for the task that produced this PR".

## Product Behavior

- Whoever starts a run mints a run id and exports it in the environment before spawning the agent CLI.
- Tools that observe the run (hooks, scanners, adapters) record a **binding**: run id ↔ native session identity (Claude Code session UUID, Codex thread id), plus pid/cwd/timestamps.
- Bindings live in a shared append-only local file. Any tool can join its own data to the run id by reading it. No daemon, no network service.
- A run started outside any orchestrator (bare terminal) simply has no run id; every tool behaves exactly as it does today.

## MVP Scope

1. The identity spec itself (this document + tech spec): ID format, env contract, binding registry format.
2. harness: mint and export the run id when spawning Claude Code / Codex CLI; stamp it on task/thread events in harness-observe.
3. A reference hook snippet (Claude Code `SessionStart` / Codex equivalent) that appends the binding record.

## Follow-Up Scope

- keepline: read bindings (and `ps -Eww` fallback) to group sessions by run id and show harness task context.
- ccstats: join cost rows to run ids via bindings.
- remem: include the run id in memory provenance when present in the environment.
- Nested runs (agent spawning agent) surfaced as parent/child chains in keepline.

## Non-Goals

- No central identity service or daemon. The registry is a local file.
- No changes to the native CLIs' own session ID formats or JSONL layouts.
- No cross-machine identity; scope is one machine, one user.
- Does not decide *what* tools store — only how they key it.

## Acceptance Criteria

- A harness-spawned Claude Code run produces: env var set in the child process, one binding line in the registry, and harness events stamped with the run id.
- A bare-terminal run (no run id) causes zero behavior change and zero errors in all tools.
- Two concurrent runs never share or overwrite each other's binding lines.
- Reading tools tolerate a registry containing malformed lines (skip and continue).

## Open Questions

- Should keepline also act as a minter when it *recovers* a session (new run id vs inheriting the lost run's id)? Leaning: recovery inherits, with a `recovered_from` marker.
- Registry rotation policy: size-based cap vs age-based pruning.
