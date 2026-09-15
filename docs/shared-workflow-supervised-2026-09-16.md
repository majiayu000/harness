# Shared workflow supervised verification — 2026-09-16

## Scope and revisions

The operator authorized sequential PR delivery, backlog disposition, and bounded
real workflow validation. PRs #2057 and #2058 were independently reviewed, passed
their latest CI Result checks, and squash-merged as `8a9b350b` and `eab26974`.

The trial used one isolated Harness checkout at `eab26974`, the selected
`config/workflows/review.md` definition (`repository_review`), a disposable
PostgreSQL 17 database, and the sanitized launcher. Intake, automatic merge,
and periodic review were disabled. Tasks were submitted through
`POST /api/workflows/runtime/submissions`, one at a time, with a 300-second
turn limit and zero failed-activity retries. Codex was requested as `gpt-5.5`
with `xhigh` reasoning; the CLI identified itself as `0.154.0`.

The two real tasks were source-only verification of the missing eval executor
(#1768) and the remaining module-layout cleanup (#1955). These are operating
checks, not code-generation or PR-repair benchmarks.

## Attempts, including unsuccessful work

| Attempt | Submission | Runtime status | Seconds | Recorded tokens |
| --- | --- | --- | ---: | ---: |
| 1 | `cb076ea2-fb02-4271-9752-3e4f6ccca0bf` | failed | 8.4 | 0 |
| 2 | `683a2a11-b359-4bb3-86c8-19ef7ebef06e` | failed | 8.7 | 0 |
| 3 | `90b784b4-cc0d-442b-a59c-8af2b0a36ddf` | failed | 303.4 | 0 |
| 4 | `f456ae79-768d-49f6-a851-96dd8b084960` | done | 85.3 | 100,541 |
| 5 | `768eb7bb-91ee-41b4-8a1a-ccdf39a2a087` | done | 101.6 | 100,584 |
| 6 | `4a229363-b19c-47b2-b70e-22b887a88d70` | done | 225.7 | 1,448,406 |
| 7 | `1bc10d0a-f891-41c7-ad18-6f17d7d89273` | done | 154.6 | 258,531 |

- Attempts 1–2 failed before a model result. Harness's restricted macOS
  workspace-write sandbox caused Codex to abort with `failed to allocate a
  guard page: Invalid argument (os error 22)`. A standalone restricted-policy
  handshake reproduced it; direct and allow-default handshakes succeeded.
- Attempt 3 used the documented full host mode, but model requests failed until
  the turn timeout. Logs identified `chatgpt.com`, absent from the example
  network allowlist. The isolated config was corrected to include that host.
  This is not evidence that the restricted sandbox works.
- Attempts 4–5 were runtime `done` but failed task acceptance: both reports
  reviewed the empty branch diff rather than the submitted source questions.
  They must not be counted as successful work.
- Attempts 6–7 used this PR's prompt-reference fix, the same source questions,
  and the same selected workflow. Both completed the requested inspections
  without in-task intervention or retry jobs.

Initial setup also corrected an incomplete TOML agent configuration and the
runtime-kind spelling to `codex_jsonrpc`. All setup corrections are operator
interventions. A dedicated worktree/database does not make full host execution
a filesystem security boundary.

## Root cause and bounded fix

Ordinary declarative activity commands omitted the submission's `prompt_ref`.
The executor's prompt loader additionally admitted only `implement_prompt`.
Consequently, submission text was durably stored and visible through the
submission prompts API, but never supplied to the ordinary custom activity.

Ordinary declarative commands now carry their existing submission reference.
The existing loader resolves commands carrying that reference regardless of
activity name, including durable recovery after a cache miss. Missing payloads
retain explicit blocking behavior. The pinned agent-contract execution path
is unchanged. No task-success classifier or new storage mechanism was added.

The initial-command regression failed before the fix (null instead of the
expected reference). Afterward, 8 filtered declarative interpreter tests and
2 prompt-loading tests passed; the latter cover built-in and custom activities
against disposable PostgreSQL. Workspace all-target check, formatting, and
independent source review passed. Latest-head CI Result is a separate merge gate.

## Accepted findings and limits

Attempt 6 correctly identified server-side host registration/claim/renew/complete
APIs and the absence of a runnable external eval host client. The local worker
rejects `RemoteHost`; scheduled preflight expects an already capable host.
The full live benchmark baseline and recurring enforcement in #1768 remain open.

Attempt 7 reported 64 `#[path]` attributes across 28 server source files. An
independent `rg` recount matched both numbers. `task_queue` uses conventional
modules already. Workspace and reconciliation remain possible future-touch
cleanup families; no batch refactor is scheduled.

A controlled restart preserved all seven full submission responses and identical
counts: 7 runtime jobs, 33 runtime events, 7 usage rows, zero pending/running jobs.
Both accepted reports declared no repository edits; terminal cleanup removed
the agent workspace before a separate final diff check. The temporary server
was stopped and its database archived. Production services were not changed.

All attempts together recorded 1,908,062 tokens, including cached input.
The two accepted attempts recorded 1,706,937 tokens, of which 1,475,712 were
cached input. Every attempt has `cost_usd_observed=false`; numeric zero is not
measured spend. No USD ceiling or provider billing reconciliation is claimed.
The trial does not establish general reliability, repair convergence, review
precision/recall, or a full benchmark baseline.

Local evidence (not a portable published artifact):
`/Users/apple/harness-supervised-20260916-i5o5aocg` contains request and response JSON, activity artifacts, server logs,
the candidate patch/binary digest, and the archived disposable database.
