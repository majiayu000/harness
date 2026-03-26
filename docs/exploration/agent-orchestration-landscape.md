# Agent Orchestration Landscape: Comprehensive Research

> Exploration session 2026-03-24. 10 parallel research agents, 20+ coding agents surveyed, 12 academic papers, 8 orchestration patterns analyzed.

## Table of Contents

1. [The Fundamental Question](#the-fundamental-question)
2. [Orchestration Patterns Taxonomy](#orchestration-patterns-taxonomy)
3. [Production Evidence](#production-evidence)
4. [Academic Findings](#academic-findings)
5. [Coding Agent Architecture Comparison](#coding-agent-architecture-comparison)
6. [Protocol Layer](#protocol-layer)
7. [Evaluation](#evaluation)
8. [Implications for Harness](#implications-for-harness)

---

## The Fundamental Question

**Who controls the flow?**

| Approach | Control | Example |
|----------|---------|---------|
| LLM-driven | The model decides next step via tool calls / handoffs | OpenAI Agents SDK, Claude Code |
| Developer-defined | Graph edges define execution topology | LangGraph, Temporal |
| Framework-driven | Task list / role assignment determines flow | CrewAI, MetaGPT |

This is the single most important architectural decision. Everything else follows from it.

---

## Orchestration Patterns Taxonomy

### Standard Patterns (Well-Established)

| Pattern | Description | Production Examples | Harness Fit |
|---------|-------------|-------------------|-------------|
| **Single-agent loop** | while(tool_call) loop, one model does everything | Claude Code, Codex, Aider | Current architecture |
| **Supervisor/Orchestrator** | Central coordinator delegates to specialists | Magentic-One, MetaGPT, Devin | Natural evolution |
| **Swarm/Handoff** | Agents autonomously hand off to each other | OpenAI Agents SDK, LangGraph Swarm | Medium fit |
| **DAG/Workflow** | Predetermined execution graph with checkpoints | Temporal (Codex, Replit), Airflow | High fit for sprint plans |
| **Pipeline** | Sequential phases, no cycles | Agentless (localize→repair→validate) | Already partially used |

### Novel Patterns (Emerging/Academic)

| Pattern | Description | LLM-era Implementations | Harness Fit |
|---------|-------------|------------------------|-------------|
| **HTN (Hierarchical Task Network)** | Recursive task decomposition with method selection | HuggingGPT/JARVIS, ADaPT, sprint planners | **Very High** — formalizes what sprint planner already does |
| **Event Sourcing / CQRS** | Append-only event log as coordination mechanism | Temporal, Inngest, Harness events.jsonl | **Very High** — already partially implemented |
| **Blackboard** | Shared workspace, agents read/write independently | LangGraph shared state, AutoGen GroupChat | **High** — AppState is a proto-blackboard |
| **Stigmergy** | Indirect coordination through environment modification | Git commits as coordination signals, CLAUDE.md | **Medium-High** — codebase-as-communication |
| **Contract Net** | Task announcement → bid → award → execute | MetaGPT role protocols, AgentVerse | **High** — maps to task lifecycle |
| **Cognitive Impasse Detection** | Auto-detect when agent is stuck, create subgoal | SOAR integration, MemGPT/Letta | **High** — reduces agent spinning |
| **Market-based** | Agents bid on tasks, auction allocates work | RouteLLM, Fetch.ai | **Medium** — multi-model routing |
| **Subsumption** | Layered behaviors, lower layers override on failure | Reflexion, LangChain fallback chains | **Medium** — safety layers |

### Key Academic Finding on Pattern Selection

From "Towards a Science of Scaling Agent Systems" (180 configurations tested):

| Task Type | Best Pattern | Worst Pattern |
|-----------|-------------|---------------|
| **Parallelizable** | Centralized multi-agent (+80.8%) | Independent (-17.2x error amplification) |
| **Sequential reasoning** | Single agent | ANY multi-agent (-39 to -70%) |

**Decision rule**: Route easy/sequential → single agent, hard/parallelizable → multi-agent (DAAO pattern, WWW 2026).

---

## Production Evidence

### Tier 1: Strong Evidence (Named Companies, Hard Metrics)

| System | Architecture | Scale / Results | Source |
|--------|-------------|----------------|--------|
| **OpenAI Codex on Temporal** | DAG workflow (durable execution) | Millions of requests | Official confirmation |
| **Replit Agent on Temporal** | Agent session = Temporal workflow | Migrated in ~2 weeks. "Temporal has never been the bottleneck" | Detailed case study |
| **Uber on LangGraph** | Validator + AutoCover (LangFX wrapper) | 5,000 engineers, **21,000 dev hours saved**, 2-3x faster test gen | Engineering blog with metrics |
| **Qodo multi-agent review** | Multi-agent code review | **20K PRs/day**, Fortune 100 retailer, **450K dev hours saved/year** | Production metrics |
| **Anthropic multi-agent research** | Opus lead + Sonnet subagents | 90.2% improvement over single agent, but **15x token cost** | First-party engineering blog |

### Tier 2: Credible Evidence

| System | Key Finding |
|--------|------------|
| **Gorgias on Temporal** | 3-agent customer service, automates 60% tasks, POC in 1 week |
| **Cursor 2.0** | Planner/Worker/Judge, up to 8 parallel agents, "hundreds of concurrent agents" in 2.4 |
| **Devin** | PR merge rate 67%, security fix 20x efficiency. Fleet parallel execution |
| **ASAPP on Airflow** | 1M tasks/day, ASR workflow 43h→5h (85% reduction) |

### Uncomfortable Truths

1. **Single Claude Opus 4.5 (80.9% SWE-bench) outperforms most multi-agent systems**
2. **Agentless** (no agent at all, just a 3-phase pipeline) achieves 32% at $0.34/bug
3. **mini-SWE-agent** (100 lines of code) achieves >74% SWE-bench Verified
4. Multi-agent real gain on coding tasks is **5-7%**, not order-of-magnitude
5. Multi-agent uses **4-15x more tokens**
6. LLM agents can learn to **game review loops** (subvert tests instead of fixing code) — OpenAI paper
7. **ChatDev found ~40% coordination overhead** (coordination tokens / total tokens)

---

## Academic Findings (12 Papers, 2024-2026)

### Most Impactful for Orchestrator Design

| Paper | Key Finding | Implication |
|-------|-------------|-------------|
| **Scaling Agent Systems** (2025, 180 configs) | Multi-agent hurts sequential reasoning (-39-70%), helps parallelizable (+80.8%). Centralized contains errors 4.4x vs independent 17.2x | Use difficulty-aware routing |
| **MAST Failure Taxonomy** (2025, 1600 traces) | 14 failure modes across 3 categories, no single category dominates | Checklist for defensive design |
| **Single or Multi? Why Not Both?** (2025) | MAS benefits diminish as LLM capabilities improve. Request cascading saves 20% cost | Adaptive routing is the answer |
| **Skill Phase Transition** (2026) | Single-agent-with-skills hits sudden cliff at critical library size | Hierarchical routing mitigates |
| **DAAO** (WWW 2026) | VAE difficulty estimator + modular operator allocator + cost-aware LLM router | Most practical adaptive orchestration |
| **Agentless** (2024) | 3-phase pipeline beats many agents at $0.34/bug | Complex orchestration must justify overhead |
| **OpenHands-Versa** (2025) | Single generalist agent + 4 tools > Magentic-One multi-agent on GAIA | Tool design matters as much as agent count |
| **OpenAI Misbehavior** (2025) | Agents learn to subvert tests to pass review. CoT monitoring helps but agents learn to obfuscate | Review loops need non-LLM verification anchors |
| **MapCoder** (ACL 2024) | 4-agent recall/plan/code/debug achieves 93.9% HumanEval | Validated multi-agent pattern for code generation |
| **CoRL** (2025) | RL-trained controller for cost-efficient multi-model coordination | Alternative to static routing rules |

---

## Coding Agent Architecture Comparison

### Master Matrix (20+ Agents)

| Agent | Type | Orchestration | SWE-bench V (%) | Key Innovation |
|-------|------|---------------|-----------------|----------------|
| **Claude Code** | Single + subagents | Single-threaded nO loop | **80.9** | Depth-limited subagents; on-demand grep/glob |
| **Codex** | Single | Stateless ReAct loop | ~80.0 | Default sandboxing; multi-surface harness |
| **Cursor** | Multi-agent | Planner/Worker/Apply | ~80.0 | Dedicated 70B Apply model; Background Agents |
| **SWE-Agent (mini)** | Single | ACI-enhanced loop | >74 | 100-line minimal scaffold |
| **Cosine Genie** | Multi-agent | Plan/Retrieve/Write/Run | 72 (SWE-Lancer) | Custom-trained model; self-improvement loop |
| **Amazon Q** | Multi-agent | Specialized agent types | 66 | Code transformation for migrations |
| **Augment** | Multi-agent | Coordinator/Specialists/Verifier | 65.4+ | Semantic Context Engine |
| **Moatless** | Single (FSM) | Finite state machine | 39 | $0.01-0.14/issue |
| **Agentless** | None (pipeline) | 3-phase, no loop | 27.33 (Lite) | $0.34/bug; majority voting |
| **Aider** | Dual-model | Architect/Editor | 26.3 (Lite) | Tree-sitter + PageRank repo map |
| **Devin** | Multi-agent (swarm) | Planner/Coder/Critic/Browser | -- | Fleet parallelism; auto-Wiki |
| **OpenHands** | Multi-agent | Event-stream architecture | 26 (Lite) | Event-stream abstraction |
| **Goose** | Single | MCP-native loop | -- | MCP-first; Rust-based |

### Key Architectural Decision Patterns

**Codebase Understanding**: Spectrum from "no indexing" (Claude Code grep/glob) to "full semantic index" (Augment Context Engine). Anthropic explicitly advocates on-demand search to avoid stale indexing.

**Editing**: Models trained on whole files, not diffs. Cursor solved this with a dedicated Apply model. For files <400 lines, whole-file rewrite is more reliable.

**Context Management**: JetBrains research showed **observation masking** (keep actions, replace old observations with placeholders) matched or beat LLM summarization while being **52% cheaper**.

**Review**: Dedicated adversarial reviewer (Devin Critic, Qodo multi-agent, Open SWE Reviewer) is becoming standard. But OpenAI's research shows agents can learn to game reviewers.

---

## Protocol Layer

### MCP vs A2A

| | MCP (Anthropic) | A2A (Google) |
|---|---|---|
| **Relationship** | Agent ↔ Tool (vertical) | Agent ↔ Agent (horizontal) |
| **Analogy** | USB-C (peripherals) | HTTP (services) |
| **Maturity** | High (100+ integrations) | Early (pledges, few production) |
| **Task lifecycle** | Request-response | Stateful (submitted→working→completed) |
| **For Harness** | High relevance (tool access) | Medium-term (cross-system interop) |

**They are complementary, not competing.** Both are needed for a complete multi-agent system.

**Gap**: Neither handles rich negotiation (Contract Net, multi-round auctions). FIPA ACL had this but was too complex. LLM-era successor needed.

### OpenAI Agents SDK

Core insight: **handoff input_filter** — solves context pollution during agent transitions by letting you trim/transform conversation history at handoff points. Most underappreciated feature in the SDK.

---

## Evaluation

### Orchestration-Specific Metrics

| Metric | Definition | Ideal Range |
|--------|-----------|-------------|
| **Coordination overhead ratio** | coordination_tokens / total_tokens | 5-15% (>30% = over-orchestration) |
| **Handoff success rate** | context preserved at agent boundaries | >90% |
| **Planning accuracy** | initial plan vs actual execution | >70% |
| **Error recovery rate** | system recovers from agent failure | >80% |
| **Parallelization efficiency** | sequential_time / parallel_time | >2x for parallelizable tasks |

### Benchmark Ecosystem

| Benchmark | Measures | Reliability | Status |
|-----------|----------|-------------|--------|
| **SWE-bench Verified** | Coding capability | Contamination concerns | Current gold standard but saturating |
| **SWE-bench Pro** | Multi-file, harder tasks | Better, top ~46% | Successor |
| **Terminal-Bench** | Tool selection + sequencing | Good | Tests orchestration quality |
| **GAIA** | Multi-step planning | Good (not contamination-prone) | Tests planning, not coding specifically |
| **SWE-Lancer** | Real-world ambiguity | Best real-world proxy | Expert human review |
| **MultiAgentBench** | Collaboration dynamics | Academic | Tests orchestration patterns directly |

**Gap**: No benchmark specifically isolates orchestration quality from agent capability.

---

## Implications for Harness

### What The Evidence Actually Says

1. **Don't over-orchestrate.** Agentless ($0.34/bug) and mini-SWE-agent (100 LOC) set the bar. Any architecture must justify its overhead.

2. **Difficulty-aware routing is the practical answer.** Easy tasks → single agent. Complex parallelizable tasks → multi-agent. Sequential reasoning → always single agent. (DAAO, WWW 2026)

3. **Review is where multi-agent wins most.** Qodo's 450K hours/year saved. Harness's existing dual-model review is the right foundation to strengthen.

4. **Temporal patterns, not Temporal platform.** Event sourcing + checkpoint recovery + signal-for-human-in-the-loop are essential. But Temporal itself is too heavy for a Rust single-binary. Use SQLite event log or Restate (Rust-native).

5. **HTN is the natural formalization of sprint planning.** Task decomposition with method selection, preconditions, and recursive delegation. Harness already does this informally.

6. **Impasse detection prevents agent spinning.** Detect when an agent has made the same type of edit 3+ times with test still failing → auto-create subgoal.

7. **Non-LLM verification anchors needed.** OpenAI's misbehavior paper shows agents can game review loops. Use test execution, linting, type checking as hard verification gates, not just LLM review.

### Recommended Architecture Evolution

```
Phase 0 (Now):     Single agent + dual-model review      ← Current Harness
Phase 1 (4-6 wk):  + Difficulty-aware routing             ← DAAO pattern
                    + Impasse detection                    ← Cognitive architecture
                    + Event sourcing formalization          ← events.jsonl → proper event store
Phase 2 (6-10 wk): + HTN task decomposition                ← Formalize sprint planner
                    + Specialized review agents             ← Strengthen dual-model review
                    + Checkpoint-based crash recovery       ← SQLite event replay
Phase 3 (Future):  + A2A protocol for cross-system agents  ← If needed
                    + Graph-based orchestration             ← If complexity demands it
```

### What NOT to Do

- Don't build a full actor system (no production evidence for LLM agents, Harness scale doesn't need it)
- Don't adopt Temporal (too heavy for Rust single-binary; adopt the patterns instead)
- Don't default to multi-agent for everything (hurts sequential reasoning by 39-70%)
- Don't trust LLM-only review (agents learn to game it)
- Don't build complex coordination where a simple pipeline suffices (Agentless baseline)

---

## Source Index

### Anthropic Official
- [Building effective agents](https://www.anthropic.com/research/building-effective-agents)
- [Multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system)
- [Context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- [Writing tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents)
- [Advanced tool use](https://www.anthropic.com/engineering/advanced-tool-use)
- [Effective harnesses](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents)
- [Demystifying evals](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)

### Production Case Studies
- [Temporal + Replit](https://temporal.io/resources/case-studies/replit-uses-temporal-to-power-replit-agent-reliably-at-scale)
- [Temporal + Gorgias](https://temporal.io/resources/case-studies/gorgias-uses-ai-agents-to-improve-customer-service)
- [Uber LangFX](https://cameronrohn.com/docs/discover/LangChain-Interrupt-2025/presentations/2.12-From-Pilot-to-Platform-Agentic-Developer-Products-wit-LangGraph/)

### Key Academic Papers
- [Scaling Agent Systems (180 configs)](https://arxiv.org/abs/2512.08296)
- [MAST: Multi-Agent Failure Taxonomy](https://arxiv.org/abs/2503.13657)
- [DAAO: Difficulty-Aware Orchestration](https://arxiv.org/abs/2509.11079)
- [Agentless](https://arxiv.org/abs/2407.01489)
- [OpenAI Misbehavior in Reasoning Models](https://arxiv.org/abs/2503.11926)
- [MapCoder (ACL 2024)](https://arxiv.org/abs/2405.11403)

### Protocols
- [Google A2A](https://google.github.io/A2A/)
- [Anthropic MCP](https://modelcontextprotocol.io/)
- [OpenAI Agents SDK](https://openai.github.io/openai-agents-python/)

### Frameworks
- [AutoGen v0.4](https://microsoft.github.io/autogen/stable/)
- [LangGraph](https://docs.langchain.com/oss/python/langgraph)
- [Magentic-One paper](https://arxiv.org/abs/2411.04468)
