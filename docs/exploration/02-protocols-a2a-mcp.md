# Agent Communication Protocols: MCP, A2A, and the Standards Landscape

## 1. MCP — Model Context Protocol (Anthropic)

**Direction**: Agent-to-Tool (vertical integration)

MCP standardizes how an AI agent connects to external tools, data sources, and services. It is the "USB-C for AI" — a universal connector between models and capabilities.

### Architecture

```
┌─────────────┐     JSON-RPC 2.0      ┌─────────────┐
│  MCP Client  │ ◄──────────────────► │  MCP Server  │
│  (AI Agent)  │   stdio / HTTP+SSE   │  (Tool/Data) │
└─────────────┘                        └─────────────┘
```

- **Transport**: stdio (local processes) or HTTP with Server-Sent Events (remote)
- **Protocol**: JSON-RPC 2.0 — request/response with optional notifications
- **Stateful sessions**: client and server maintain connection state

### Three Primitives

| Primitive | Direction | Purpose | Example |
|-----------|-----------|---------|---------|
| **Tools** | Client → Server | Execute actions | `create_github_issue(title, body)` |
| **Resources** | Server → Client | Expose data | `file:///repo/src/main.rs` |
| **Prompts** | Server → Client | Provide templates | `code-review-prompt(diff)` |

### Maturity

- **100+ integrations** across major vendors (VS Code, JetBrains, Cursor, Claude Desktop)
- **Spec version**: 2025-03-26 (latest stable)
- **SDKs**: TypeScript, Python, Java, Kotlin, C#, Swift, Rust (community)
- **Adoption**: De facto standard for agent-tool connectivity

### Strengths

- Simple to implement (a server is ~50 lines)
- Tool discovery via `tools/list` — agents can enumerate available capabilities
- Sampling: server can request LLM completions from client (bidirectional)
- Roots: client declares filesystem boundaries server can access

### Limitations

- **Agent-to-agent**: MCP does not address how two agents communicate with each other
- **Discovery**: No standard registry for finding MCP servers (manual configuration)
- **Authentication**: OAuth 2.1 added in March 2025, but adoption is early
- **Versioning**: No built-in schema versioning for tool interfaces
- **Streaming results**: Tools return complete results, no incremental/streaming tool output

---

## 2. A2A — Agent-to-Agent Protocol (Google, April 2025)

**Direction**: Agent-to-Agent (horizontal coordination)

A2A standardizes how independent agents discover each other and collaborate on tasks. It is "HTTP for agents" — enabling service-to-service agent communication.

### Architecture

```
┌──────────────┐                    ┌──────────────┐
│   Agent A     │                    │   Agent B     │
│  (Client)     │   A2A Protocol     │  (Server)     │
│               │ ──────────────────►│               │
│  Sends Task   │   HTTP + JSON-RPC  │  Agent Card   │
│  Gets Updates │ ◄──────────────────│  at /.well-   │
│               │   SSE Streaming    │  known/agent  │
└──────────────┘                    └──────────────┘
```

### Core Concepts

#### Agent Card (Discovery)

Every A2A-compliant agent publishes a JSON document at `/.well-known/agent.json`:

```json
{
  "name": "Code Review Agent",
  "description": "Reviews pull requests for bugs and style",
  "url": "https://review-agent.example.com",
  "version": "1.0.0",
  "capabilities": {
    "streaming": true,
    "pushNotifications": true
  },
  "skills": [
    {
      "id": "review-pr",
      "description": "Review a GitHub pull request",
      "inputModes": ["text/plain", "application/json"],
      "outputModes": ["text/plain", "text/markdown"]
    }
  ],
  "authentication": {
    "schemes": ["bearer"]
  }
}
```

#### Task Lifecycle

```
submitted → working → input-required → completed
                  ↘                    ↗
                    → failed/canceled
```

- **submitted**: Client sends task to server agent
- **working**: Server agent is processing (can stream partial results via SSE)
- **input-required**: Server agent needs additional information from client
- **completed**: Task finished with result artifacts
- **failed/canceled**: Terminal error states

#### Artifacts and Parts

Tasks produce **Artifacts** containing **Parts**:

```json
{
  "artifacts": [
    {
      "name": "review-report",
      "parts": [
        {"type": "text/markdown", "content": "## Review Summary\n..."},
        {"type": "application/json", "content": {"issues": 3, "severity": "medium"}}
      ]
    }
  ]
}
```

### Maturity

- **Early stage**: Released April 2025, limited production adoption
- **Supported by**: Google, Salesforce, SAP, Atlassian, LangChain (announced, varying implementation depth)
- **SDKs**: Python, TypeScript (reference implementations)
- **Spec**: Open specification, Apache 2.0 license

### Strengths

- Standard discovery mechanism (Agent Cards)
- Rich task lifecycle with intermediate states
- SSE streaming for long-running tasks
- Content negotiation via MIME types
- Push notifications for async completion

### Limitations

- **No rich negotiation**: Cannot express "I'll do this task for X cost" or "I bid on this task"
- **No multi-party**: Strictly bilateral (one client, one server per task)
- **No orchestration**: A2A does not prescribe how to coordinate multiple agents
- **No shared state**: Each task is independent, no cross-task state management
- **Young ecosystem**: Few production deployments

---

## 3. MCP vs A2A: Complementary, Not Competing

The two protocols address different communication axes:

```
                        ┌─────────────┐
                        │   Agent A    │
                        └──────┬──────┘
                    A2A        │        A2A
                (horizontal)   │    (horizontal)
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
        ┌──────────┐    ┌──────────┐    ┌──────────┐
        │  Agent B  │    │  Agent C  │    │  Agent D  │
        └─────┬────┘    └─────┬────┘    └─────┬────┘
              │               │               │
         MCP  │          MCP  │          MCP  │
      (vertical)      (vertical)      (vertical)
              │               │               │
        ┌─────▼────┐    ┌─────▼────┐    ┌─────▼────┐
        │  DB Tool  │    │ Git Tool  │    │ API Tool  │
        └──────────┘    └──────────┘    └──────────┘
```

| Dimension | MCP | A2A |
|-----------|-----|-----|
| **Analogy** | USB-C (peripherals) | HTTP (services) |
| **Relationship** | Agent ↔ Tool | Agent ↔ Agent |
| **Statefulness** | Session-based | Task-based |
| **Discovery** | Manual config | Agent Card (`.well-known`) |
| **Communication** | Synchronous RPC | Async task lifecycle |
| **Streaming** | SSE for notifications | SSE for task updates |
| **Content model** | Tool results (JSON) | Artifacts with MIME parts |
| **Authentication** | OAuth 2.1 | Bearer/OAuth (Agent Card) |

### Combined Usage Pattern

A realistic multi-agent system uses both:

1. **Agent A** receives user request
2. **Agent A** uses **MCP** to query database tool for context
3. **Agent A** uses **A2A** to delegate subtask to **Agent B**
4. **Agent B** uses **MCP** to call GitHub tool for implementation
5. **Agent B** returns result artifact to **Agent A** via **A2A**
6. **Agent A** uses **MCP** to write result to storage tool

---

## 4. The Negotiation Gap: What Neither Protocol Handles

Both MCP and A2A model simple request-response interactions. Neither supports:

- **Bidding/Auctions**: "Which agent can do this task cheapest/fastest?"
- **Contract negotiation**: "I'll do X if you provide Y"
- **Multi-party agreements**: Three agents agreeing on a shared plan
- **Capability matching**: "Find me an agent that can handle TypeScript + React + testing"
- **Service-level commitments**: "I guarantee completion within 5 minutes"

### FIPA ACL: The Road Not Taken

The Foundation for Intelligent Physical Agents (FIPA) defined Agent Communication Language (ACL) in the late 1990s with exactly these capabilities:

| FIPA Concept | Modern Revival | Status |
|---|---|---|
| **Directory Facilitator** | A2A Agent Cards | Partial — discovery yes, matchmaking no |
| **Interaction Protocols** | A2A Task states | Partial — lifecycle yes, negotiation no |
| **Contract Net Protocol** | Nothing standard | Gap — no agent bidding/auction mechanism |
| **Performatives** (inform, request, propose, accept) | Nothing standard | Gap — only request/respond exists |
| **Content Language** | JSON/MIME types | Good — simpler than FIPA-SL |
| **Ontology Service** | Nothing standard | Gap — no shared vocabulary negotiation |

FIPA failed because it was **too complex** — implementing a compliant agent required understanding 25+ specifications. The modern approach (MCP + A2A) succeeds by being minimal, but leaves negotiation as a gap.

### Why the Gap Matters

For simple delegation ("do this task"), A2A is sufficient. But for:
- **Cost optimization**: routing tasks to cheapest capable agent
- **Load balancing**: distributing work based on agent capacity
- **Quality bidding**: choosing agent based on confidence score
- **Multi-step negotiation**: agents agreeing on scope before execution

...there is no standard protocol. Systems that need this (Fetch.ai, MetaGPT) build custom solutions.

---

## 5. Google ADK (Agent Development Kit)

Google's ADK bridges the protocol gap by providing a framework that supports both MCP and A2A:

### Multi-Agent Hierarchy

```
┌─────────────────────────────┐
│       Root Agent             │
│  (Orchestrator)              │
├─────────┬───────────────────┤
│ Sub-Agent│ Sub-Agent│ Sub-Agent│
│  (MCP)   │  (A2A)   │  (LLM)  │
└─────────┴───────────┴─────────┘
```

- **Root agent** coordinates sub-agents
- Sub-agents can be:
  - **LLM agents** (direct model calls)
  - **MCP tool agents** (connect to MCP servers)
  - **A2A remote agents** (delegate to external agents)
- Automatic tool routing based on sub-agent capabilities
- Session management with built-in persistence

### Key Features

- Unified interface for local and remote agents
- Built-in evaluation framework
- Streaming support across all agent types
- Artifact management for multi-modal outputs

---

## 6. Standards Landscape

### Current State

| Organization | Effort | Status |
|---|---|---|
| Anthropic | MCP specification | Active, de facto standard for agent-tool |
| Google | A2A specification | Active, early adoption |
| Google | ADK framework | Active, supports both MCP + A2A |
| OpenAI | Agents SDK | Active, proprietary framework (not a protocol) |
| OASIS | None | No agent communication TC |
| W3C | None | No agent protocol working group |
| IEEE | P3394 (Agent Interop) | Proposed, not yet chartered |

### What's Missing

1. **No W3C or OASIS standard** in progress for agent-to-agent communication
2. **No standard agent registry** (like DNS for agents)
3. **No standard negotiation protocol** (auctions, bidding, contracts)
4. **No standard for agent identity** (beyond Agent Card self-description)
5. **No standard for agent trust/reputation** (how to assess unknown agents)

### Prediction

The likely evolution:
1. **MCP** becomes the universal agent-tool standard (already happening)
2. **A2A** becomes one of several agent-agent protocols (competition from Anthropic, Microsoft)
3. **Negotiation** remains application-specific for 2-3 years
4. **Standards bodies** engage only after de facto consolidation (2026-2027)

---

## 7. Implications for Harness

| Protocol | Harness Relevance | Action |
|---|---|---|
| MCP | Agents already use MCP tools via Claude Code | No change needed — MCP is the agent's concern |
| A2A | Could expose Harness tasks as A2A-compatible endpoints | Future — when inter-system agent coordination needed |
| Agent Card | Could publish Harness capabilities as discoverable Agent Card | Low priority — single-system for now |
| Task lifecycle | A2A's submitted→working→completed maps to Harness task states | Validate alignment, consider adopting state names |

### Key Insight

Harness operates **above** both protocols: it orchestrates agents that use MCP for tools and could use A2A for inter-agent communication. The orchestrator itself doesn't need to implement either protocol — it manages the agents that do.
