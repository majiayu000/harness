# Real-World Cases: Agent Orchestration in Production

> Exploration session 2026-03-24. Web research results with confidence ratings.

## A: Actor Model Cases

| Case | Details | Confidence |
|------|---------|------------|
| **Microsoft AutoGen v0.4** | 2024 rewrite with actor model as foundation. 80k+ GitHub stars. Two-layer: Core (actor runtime) + AgentChat (high-level API). SingleThreadedAgentRuntime (asyncio queue) + DistributedAgentRuntime | **HIGH** — first-party architecture docs |
| **Akka Agentic AI Platform** | 15-year actor framework pivoting to AI agents. Customers: Walmart, Capital One, Tubi, Swiggy. Claims 80% compute reduction vs Python, 10M TPS | **LOW** — customers are legacy Akka users, not confirmed AI agent usage |
| **OpenFANG (Rust)** | 137k-line Rust "agent OS". 2400 tasks/sec vs CrewAI 180. 100 agents use 1.2GB vs CrewAI 8.4GB | **LOW** — no production deployments, benchmarks on simple routing |
| **AutoAgents (Rust, ractor)** | Ractor-based LLM agent framework. 5x less memory than Python. Latency similar (LLM dominates) | **LOW** — early stage |

**Assessment**: Actor model lacks "50k tasks/day at Company X" level evidence for LLM agents. Value is in fault tolerance and state isolation, but LLM call latency (5-10s) drowns out actor's sub-ms message passing advantage.

## B: DAG Workflow Cases

| Case | Details | Confidence |
|------|---------|------------|
| **OpenAI Codex on Temporal** | Entire execution pipeline runs on Temporal. "Millions of requests" in production | **HIGH** — official confirmation |
| **Replit Agent on Temporal** | Migrated from custom orchestration Nov 2024 in ~2 weeks. "Temporal has never been the bottleneck." "No major incidents tracing back to Temporal Cloud" | **HIGH** — detailed case study |
| **Gorgias on Temporal** | 3-agent customer service (support/sales/manager). 15,000+ e-commerce brands. Automates 60% of repetitive tasks. POC in 1 week, production in 1 month | **HIGH** — architecture details public |
| **Uber on LangGraph** | Validator + AutoCover tools. 5,000 engineers. **21,000 developer hours saved**. Test generation 2-3x faster via 100 parallel iterations | **HIGH** — hard metrics |
| **ASAPP on Airflow** | 1M tasks/day, 5K pipelines daily. ASR workflow 43h → 5h (85% reduction) | **HIGH** — but ML pipeline, not autonomous agents |
| **Netflix Maestro** | 100k+ workflows, 2M jobs/day. Open sourced July 2024. Supports DAG + cyclic workflows | **HIGH** — but data pipelines, not AI agents |

**Assessment**: **Temporal is the de facto standard** for production AI agent orchestration. OpenAI + Replit + Gorgias are the 3 strongest cases.

### ZenML 1,200-Deployment Survey Insights
- Slack uses Temporal for "workflow state management across escalation lifecycles"
- DoorDash uses orchestrators with subtask decomposition + progress tracking
- **GetOnStack cautionary tale**: undetected infinite loop between agents escalated costs from **$127/week to $47,000/week** — highlighting why DAG (acyclic) constraints matter
- Key insight: successful teams treat agents as microservices

## C: Multi-Agent Swarm Cases

| Case | Details | Confidence |
|------|---------|------------|
| **Anthropic multi-agent research** | Opus 4 lead + Sonnet 4 subagents. **90.2% improvement** over single agent. But **15x token cost**. Early failure: 50+ subagents for simple queries | **HIGH** — first-party engineering blog |
| **Cursor 2.0** | Planner/Worker/Judge pattern. Up to 8 parallel agents, isolated codebase copies. Proprietary "Composer" model | **MEDIUM** — architecture public, no benchmarks |
| **Agyn (SWE-bench)** | 4 agents (Manager/Researcher/Engineer/Reviewer). SWE-bench Verified 72.2%, +7.2% over single agent baseline | **HIGH** — controlled experiment |
| **Qodo code review** | Multi-agent review: 60.1% F1, extended mode 71%. **20K PRs/day** for Fortune 100 retailer, **450K developer hours saved/year** | **HIGH** — production metrics |
| **Devin (Cognition)** | "Fleet of Devins" parallel execution. PR merge rate 67%. Security fix efficiency 20x | **MEDIUM** — self-reported |
| **OpenAI Codex** | **Single agent loop**, NOT multi-agent. Power from specialized model + sandbox, not collaboration | **HIGH** — explicitly single agent |
| **Verdent (SWE-bench)** | Multi-agent plan-code-verify. 76.1% pass@1, 81.2% pass@3. Uses git worktrees for isolation | **HIGH** — technical report |

**Uncomfortable truth**:
- Single Claude Opus 4.5 (80.9% SWE-bench) outperforms most multi-agent systems
- Multi-agent real gain is 5-7%, not order-of-magnitude
- Biggest wins in **code review** (separable concerns) and **research** (parallelizable exploration), not code generation
- Token cost 4-15x unless using cheap models for non-critical roles

## Implications for Harness

| Dimension | Fact | Inference |
|-----------|------|-----------|
| DAG/Temporal route | OpenAI Codex and Replit both use Temporal | Harness's crash recovery + sprint plan orchestration fits DAG best |
| Multi-agent route | Largest gains in review (Qodo 450K hours/year) | Harness's existing dual-model review can be strengthened first |
| Actor model route | No production LLM agent cases | Overengineering for Harness's current scale (~tens of tasks/day) |
