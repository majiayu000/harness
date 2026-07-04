# Spawn-Failure Classification, Backoff, and Circuit Breaker — Design Spec

Status: Draft v0 (proposal, pending review)
Refs: #1478
Audience: harness-server reviewers + future implementer

## 1. Problem

The runtime respawns dead agent jobs on a fixed ~30–60s cadence with no backoff,
no failure classification, and no circuit breaker. When the failure cause is
environment-level (e.g. upstream proxy OAuth 401 `token_invalidated`), identical
retries cannot succeed, yet the runtime keeps spawning them across every
workspace, burning quota and flooding session logs.

## 2. Evidence

Audit of 132 Codex implx sessions (2026-06-30 → 2026-07-04) driven by the harness
runtime and SpecRail queue workflows. On 2026-07-02 between 18:50 and 19:25 the
runtime respawned near-identical dead Codex jobs every ~30–60s across 7
`runtime-wf-repo-backlog-*` workspaces plus `issue_1415` / `issue_1416` /
`issue_1417` — approximately 248 sessions in 35 minutes, each ending:

```json
{"type":"task_complete","last_agent_message":null,"duration_ms":4722}
```

(durations ranged 3000–13000 ms) with zero output. Root cause was
environment-level: upstream proxy OAuth 401 `token_invalidated`.

Local evidence files (audit machine):
`~/.codex/sessions/2026/07/02/rollout-2026-07-02T18-51-51-*.jsonl` through
`rollout-2026-07-02T19-23-33-*.jsonl` (14 in the audited sample; ~248 total in the
window).

## 3. Goals

- Classify spawn failures at minimum into `env_suspected` (zero-output death,
  auth/network signatures) vs `task_level` (agent ran, produced output, failed).
- Apply exponential backoff per task/workspace after consecutive zero-output
  spawn deaths, replacing the fixed respawn cadence.
- Open a runtime-wide circuit breaker when N consecutive zero-output failures
  occur across distinct tasks within a window: pause spawning and surface a
  degraded-health signal indicating a suspected environment-level cause.
- Half-open probing: after a cooldown, spawn one probe task; success closes the
  breaker, failure re-opens it with a longer cooldown.

## 4. Non-Goals

- Not detecting zero-output completions themselves — that is #1477; this spec
  consumes its classification as input.
- Not detecting long-running stalls — that is #1437.
- Not fixing any specific environment cause (proxy auth, network); the breaker
  only stops the runtime from burning retries against one.
- Not changing per-activity retry semantics for ordinary task-level failures.

## 5. Proposed Behavior

1. **Classification.** Every spawn outcome is tagged: `ok`, `task_level` failure,
   or `env_suspected` (zero-output death per #1477, or a recognizable auth/network
   error signature in the process output).
2. **Per-task backoff.** Consecutive `env_suspected` failures for the same
   task/workspace grow the respawn delay exponentially (e.g. base 30s, factor 2,
   cap 15 min, with jitter). Any successful spawn resets the counter.
3. **Global circuit breaker.** A sliding window tracks `env_suspected` failures
   across distinct tasks. When the count reaches the threshold (default: 5
   consecutive failures across ≥3 distinct tasks within 5 minutes), the breaker
   opens: no new agent spawns, queued work stays queued (not failed).
4. **Health surface.** While open, the health endpoint and status surfaces report
   a degraded state naming the breaker, the trigger counts, and the suspected
   environment-level cause. Opening and closing are logged at error level.
5. **Recovery.** After a cooldown the breaker goes half-open and admits one probe
   spawn. Probe success closes the breaker and resumes normal dispatch; probe
   failure re-opens it with the next cooldown step.

## 6. Behavior Invariants

1. Every spawn outcome MUST carry a failure classification; `env_suspected` and
   `task_level` MUST be distinguishable in task events and logs.
2. After K consecutive `env_suspected` failures of the same task, the next respawn
   delay MUST be at least `base * 2^(K-1)` (capped); the runtime MUST NOT respawn
   the same dead job on a fixed cadence.
3. When the global threshold (N consecutive `env_suspected` failures across
   distinct tasks within window W) is reached, the runtime MUST stop initiating
   new agent spawns until the breaker half-opens.
4. While the breaker is open, affected tasks MUST remain queued/non-terminal; the
   breaker MUST NOT mark them failed or succeeded.
5. Breaker state transitions (closed → open, open → half-open, half-open →
   closed/open) MUST be logged at error level and reflected in the health surface
   within one poll interval.
6. A successful spawn MUST reset the per-task backoff counter for that task; a
   successful probe MUST close the global breaker.
7. Thresholds (K, N, W, base delay, cap, cooldown) MUST be configurable with the
   defaults recorded in `config/default.toml.example`; hardcoded-only thresholds
   are not acceptable.
8. `task_level` failures MUST NOT count toward the global breaker threshold.

## 7. Acceptance Criteria

- Unit tests for the classifier: zero-output death → `env_suspected`; agent
  output present + failure → `task_level`.
- Unit tests for backoff: delay sequence grows exponentially with cap and resets
  on success (use virtual time, no real sleeps — repo convention per #1455).
- Unit tests for the breaker state machine: threshold trip, open blocks spawns,
  half-open probe success/failure paths.
- Integration test: a simulated env-level outage produces ≤ ceil(log2(outage /
  base)) + probe spawns per task instead of one spawn per fixed tick.
- Health endpoint test: open breaker reports degraded with cause annotation.
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` and `cargo test`
  pass.

## 8. Rollout Notes

- Depends on #1477 landing first (zero-output classification is the primary
  `env_suspected` signal).
- Ship the breaker behind config with conservative defaults; backoff can default
  ON immediately since it only slows retries that were already failing.
- Emit breaker/backoff counters through the existing usage/health metrics so the
  usage monitor dashboard can display pressure (relates to #1439 surfaces).
- Under the 2026-07-02 replay, the 35-minute window would have produced roughly a
  dozen spawns runtime-wide instead of ~248.
