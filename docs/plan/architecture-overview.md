# Harness Architecture Overview — Post P0/P1 Vision

## System Layers

```
┌─────────────────────────────────────────────────────────────────┐
│                        Trigger Layer                            │
│  GitHub Webhook (#106) │ CLI │ SDK (#111) │ Cron/Automations   │
└───────────────────────────────┬─────────────────────────────────┘
                                │
┌───────────────────────────────▼─────────────────────────────────┐
│                        Orchestration Layer                       │
│                                                                  │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌───────────────────┐  │
│  │AGENTS.md │ │Complexity│ │ Approval │ │   Task Pipeline   │  │
│  │ #107     │ │Router    │ │ Policy   │ │ (task_executor)   │  │
│  │          │ │ #110     │ │ #108     │ │                   │  │
│  └──────────┘ └──────────┘ └──────────┘ └───────────────────┘  │
└───────────────────────────────┬─────────────────────────────────┘
                                │
┌───────────────────────────────▼─────────────────────────────────┐
│                        Execution Layer                           │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Agent Loop (#102) + Streaming (#104) + Sandbox (#105)  │    │
│  │  ClaudeAdapter (stream-json) | CodexAdapter (App Server) │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌─────────────┐  ┌──────────┐  ┌──────────────────────────┐   │
│  │  Worktree   │  │ Protocol │  │  Review Loop             │   │
│  │  Isolation  │  │ #103     │  │  Agent Review -> GitHub  │   │
│  └─────────────┘  └──────────┘  └──────────────────────────┘   │
└───────────────────────────────┬─────────────────────────────────┘
                                │
┌───────────────────────────────▼─────────────────────────────────┐
│                     Quality Layer (existing, preserved)          │
│                                                                  │
│  ┌────────────┐ ┌────────────┐ ┌──────────┐ ┌──────────────┐   │
│  │   Rules    │ │   Skills   │ │  Learn   │ │ Interceptors │   │
│  │  Engine    │ │   Store    │ │          │ │ (Contract    │   │
│  │ 4-layer   │ │ 4-layer    │ │ learn_   │ │  Validator)  │   │
│  │ discovery │ │ discovery  │ │ rules()  │ │ pre/post     │   │
│  │ guard exec│ │ trigger    │ │ learn_   │ │ execute      │   │
│  │           │ │ patterns   │ │ skills() │ │              │   │
│  └────────────┘ └────────────┘ └──────────┘ └──────────────┘   │
│                                                                  │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │                    GC (Signal-Driven Remediation)           │ │
│  │  EventStore -> SignalDetector -> GcAgent -> Drafts         │ │
│  │  Signals: RepeatedWarning, ChronicBlock, HotFileEdits,    │ │
│  │           SlowOp, ViolationEscalation, RuleViolation      │ │
│  └────────────────────────────────────────────────────────────┘ │
└───────────────────────────────┬─────────────────────────────────┘
                                │
┌───────────────────────────────▼─────────────────────────────────┐
│                     Observability Layer                           │
│  ┌────────────┐ ┌────────────┐ ┌──────────┐ ┌──────────────┐   │
│  │ EventStore │ │  Quality   │ │  Health  │ │   OTel       │   │
│  │ (JSONL)    │ │  Grader    │ │  Report  │ │   (#115)     │   │
│  └────────────┘ └────────────┘ └──────────┘ └──────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

## Quality Layer Enhancements from P0/P1

| Subsystem | Current | After P0/P1 |
|-----------|---------|-------------|
| Rules | Scan only after task completes | Rule check on every tool call via Interceptor pre_execute |
| Skills | Bulk inject into prompt | Context-aware injection via AGENTS.md + trigger_patterns |
| Learn | Learn from final output | Learn from streaming events — every tool call pattern captured |
| GC | Offline signal detection from EventStore | Realtime signal detection from streaming events |
| Interceptors | pre/post at turn level | Every tool call passes interceptor chain (item-level granularity) |
| Quality Grader | Based on violation count | Add agent loop metrics: tool call count, retry rate, approval reject rate |

## Key Principle

Quality layer is harness's moat. P0/P1 changes the engine, quality layer is the instrument panel — new engine doesn't replace instruments, but instruments read more sensor data.
