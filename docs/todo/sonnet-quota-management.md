# Sonnet Quota Management

## Problem

Harness server spawns Claude Sonnet agents for every task phase (implement, review, rebase). With 18+ concurrent tasks, Sonnet 7-day quota hit 93% in one session.

## Root Causes

1. **No concurrency limit awareness**: Server spawns agents without checking API quota
2. **Review loops**: Failed tasks retry up to 8 rounds, each consuming Sonnet tokens
3. **Duplicate work**: Rebase tasks that fail get retried, doubling token spend
4. **No model fallback**: All tasks use Sonnet regardless of complexity

## Solutions

### S1: Add quota-aware throttling

Before spawning an agent, check remaining quota. If below threshold (e.g., 30%), pause non-critical tasks.

### S2: Reduce review rounds

Current: 8 review rounds max. Most issues are caught in round 1-2.
Proposed: Reduce to 3 rounds max for rebase/simple tasks.

### S3: Model tiering

- Simple tasks (rebase, fmt, version bump) → Haiku
- Standard tasks (bug fix, feature) → Sonnet
- Complex tasks (architecture, security) → Opus

### S4: Batch scheduling

Instead of processing all PRs simultaneously, queue them and process 2-3 at a time.
