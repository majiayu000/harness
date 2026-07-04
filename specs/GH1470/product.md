# Product Spec

## Linked Issue

GH-1470

## User Problem

Every project pays one full review-agent invocation per periodic-review tick
(default 2h) even when nothing changed — the agent itself reports
REVIEW_SKIPPED. Idle fleets burn agent invocations to learn "no new commits".

## Goals

- Zero agent spawns for projects with no new commits since the last completed
  review.
- Keep agent-side REVIEW_SKIPPED as defense in depth.

## Non-Goals

- Changing review cadence, scope, or content.

## Behavior Invariants

1. A periodic-review tick for a project with zero new commits since its last
   completed review spawns zero agent processes.
2. Skips are observable (log + counter), never silent.
3. When commit state cannot be determined cheaply, the reviewer falls back to
   spawning (conservative: availability of review beats saving one call).
