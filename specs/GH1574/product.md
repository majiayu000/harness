# GH1574 Product Spec: NAP-Lite Behavior-Preserving Observation Compression

GitHub issue: `#1574`

## Goals

- Cut token spend and context pressure at the two seams Harness owns:
  subagent results entering parent context, and over-budget context
  composition.
- Preserve agent behavior: a compressed observation must induce the same
  next action as the raw observation (CoACT / Next-Action Preservation,
  arXiv:2607.02911), verified by sampling.
- Never degrade silently: every compression is recorded in the compose
  manifest; NAP verification failures fall back to raw text and are counted.

## Non-Goals

- Do not compress inside the Claude Code / Codex tool loop (black box; no
  runtime fork).
- Do not train or fine-tune a compressor model.
- Do not add gateway-level (litellm-rs) compression.
- Do not enable compression by default; it is opt-in per config.

## Users

- Fleet operators running many parallel agents where subagent output volume
  dominates parent context.
- Long-queue orchestrations that hit ContextComposer quotas and currently
  lose information to truncation.

## Evidence

Measured on the 40 largest local Codex sessions (Jun-Jul 2026, 247M content
chars): observations are 89.4% of content bytes; 377 compaction events;
observations >= 2KB hold 91.5% of observation bytes (p50 597B, p90 9.5KB).

## Acceptance Criteria

- `ObservationCompressor` trait exists in `harness-core` with a prompt-based
  implementation; model/endpoint come from config; feature is OFF by default
  and inert when unconfigured.
- Observations below `min_size_bytes` (default 2048) are never compressed.
- NAP verification runs on a configurable sample (default 10%): raw and
  compressed versions each produce a structured next-action sketch
  (`intent`, `target_files`, `command_class`); 2 of 3 fields must agree.
- On NAP failure: fall back to raw text, increment `nap_failed`; when the
  task-level failure rate exceeds 15%, disable compression for that task and
  surface an error-level event (no warning+fallback).
- Compose manifest records per-item `original_tokens`, `compressed_tokens`,
  and `nap` status.
- Phase 1 gate on the 40-session replay set: >= 20% token saving,
  < 5% NAP mismatch, task success parity, compression call cost < 20% of
  saved token value. Below gate, the feature ships disabled and downgrades
  to rule-based firewall guidance only.
