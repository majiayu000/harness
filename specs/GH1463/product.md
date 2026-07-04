# Product Spec

## Linked Issue

GH-1463

## User Problem

`http/background.rs` (3,509 lines, 18 tokio::spawn sites) is a catch-all
where unrelated background loops accumulated. No single domain concept binds
the file; reading, reviewing, and safely changing any one loop requires
navigating all of them, and shutdown ordering is hard to reason about.

## Goals

- One module per background loop under `http/background/`, preserving
  behavior exactly.
- A single registry of spawned loops so "what runs in the background" is
  answerable in one place.

## Non-Goals

- Behavior changes to any loop.
- Splitting harness-server into multiple crates (separate decision).

## Behavior Invariants

1. Pure code motion: every loop keeps its interval, config gating, and
   shutdown semantics; no observable behavior change.
2. Each extracted module has a named spawn function registered in one
   background-registry list.
3. `background.rs` post-split is under the repo's standard file-size ceiling
   and the CLAUDE.md exemption for it is removed.
