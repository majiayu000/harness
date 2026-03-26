# Harness Evolution: Three Architecture Directions

> Exploration session 2026-03-24. Analysis of 3 alternative architectures for Harness.

## Overview

| Dimension | A: Event-Driven Actor Model | B: DAG Workflow Engine | C: Multi-Agent Swarm |
|-----------|---------------------------|----------------------|---------------------|
| Core idea | Each task is an independent actor, message passing replaces shared state | Task orchestration as DAG with explicit dependencies | Specialized agents collaborate (Triage/Architect/Impl/Test/Review) |
| Solves | State isolation, crash recovery, concurrency safety | Task ordering, conditional branching, workflow reuse | Context focus, model specialization, parallel collaboration |
| Analogues | Erlang/Akka supervisor trees | Temporal / Airflow / Argo | CrewAI / AutoGen / OpenAI Swarm |

## Pain Point Coverage

| Current Pain Point | Actor | DAG | Swarm |
|-------------------|-------|-----|-------|
| DashMap shared state contention | **Fully solved** — actor owns state | Partial — checkpoint replaces | Partial — per-agent context |
| Serial vs parallel execution | Solved — actors naturally parallel | **Fully solved** — fan-out/fan-in | Solved — roles can parallelize |
| Crash recovery / state loss | **Fully solved** — supervisor restart + event replay | **Fully solved** — checkpoint recovery | Partial — needs extra state mgmt |
| Review loop deadlock | Partial — explicit state machine | **Fully solved** — conditional branches + timeout | Solved — independent Reviewer agent |
| Sprint plan orchestration | Manual actor composition | **Fully solved** — YAML templates | Partial — supervisor orchestration |
| Context window pollution | Not solved | Not solved | **Fully solved** — minimal context per role |
| Model cost optimization | Not solved | Not solved | **Fully solved** — cheap models for triage |

## Engineering Cost

| Dimension | Actor | DAG | Swarm |
|-----------|-------|-----|-------|
| Implementation complexity | High — new runtime model | Medium-High — scheduler + YAML | Medium — reuses existing agent trait |
| Estimated effort | 22-31 person-days (6-8 weeks) | 15-20 person-days (5-7 weeks) | 12-16 person-days (4-6 weeks) |
| New code | ~3000 LOC | ~2000 LOC | ~1500 LOC |
| Learning curve | High — message passing mindset | Medium — DAG/workflow concepts | Low — extends existing CodeAgent |
| Migration risk | High — rewrites task_runner core | Medium — new crate, old API unchanged | Low — incremental (triage first, then add roles) |
| Debug difficulty | Medium — message tracing | High — distributed workflow state | Medium — per-agent independent logs |

## Key Differences

| | Actor | DAG | Swarm |
|---|-------|-----|-------|
| Architecture layer | Infrastructure (how tasks run) | Orchestration (what order tasks run) | Intelligence (who does what) |
| Change to Harness | Rewrite execution engine | Add orchestration layer | Split prompts into roles |
| Backward compatible | Low — API semantics change | High — single task = 1-node DAG | High — single agent = all roles |
| Unique advantage | Event sourcing, precise crash recovery | Visualizable workflows, template reuse | Token efficiency ↑23%, cost ↓13% |
| Existing code base | None — build from scratch | None — new crate | **Has** — parallel_dispatch + periodic_reviewer |

## Combinability

Three approaches are **not mutually exclusive**. Recommended progression:

```
C (Swarm) → B (DAG) → A (Actor)

Phase 1: Swarm — fastest ROI, reuses existing code, token/cost savings immediate
Phase 2: DAG — orchestrate multi-agent workflows on top of Swarm (sprint plan templates)
Phase 3: Actor — if scale demands it, refactor execution engine with actors
```

## Recommendation

| Priority | Choose |
|----------|--------|
| Fast wins + low risk | C (Swarm) — 4-6 weeks, incremental, saves on tokens |
| Solve orchestration complexity | B (DAG) — sprint plan templates, conditional branching |
| Long-term architectural robustness | A (Actor) — state isolation + crash recovery, longest timeline |
| Want all | C → B → A progressive evolution |
