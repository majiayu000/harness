# Tech Spec

## Linked Issue

GH-1451

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| OTel export | `crates/harness-observe/src/otel_export.rs` | `OtelPipeline`, metrics-only (counters/histograms) | Add a TracerProvider alongside the MeterProvider |
| Run lifecycle | `crates/harness-workflow/src/runtime/{dispatcher,worker,reducer}` | State transitions persisted to Postgres | Span open/close hooks |
| Turn telemetry | `crates/harness-agents/src/{claude,codex}.rs`, usage parsing (`harness-observe/src/usage.rs`) | Token usage parsed from agent output | Turn-span attributes |
| Config | `crates/harness-core/src/config/` (otel section) | exporter/endpoint/environment | New `otel.trajectory`, `otel.capture_content` flags |
| Activity results | `crates/harness-server/src/workflow_runtime_worker/activity_result.rs` | Structured outcome parsing | Outcome attributes |

## Proposed Design

1. **Tracer wiring** — extend `OtelPipeline` with an SDK `TracerProvider`
   sharing the existing exporter config; gated by `otel.trajectory`.
2. **Span model** —
   - root: `harness.workflow` (workflow id, repo, trigger, terminal outcome);
   - child: `harness.activity` per activity execution (kind, attempt, state);
   - child: GenAI turn span per agent invocation with semconv attributes
     (`gen_ai.system` (adapter), `gen_ai.request.model`,
     `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`) plus
     `harness.cost_usd` where computable.
3. **Context propagation** — trace context stored on the workflow row when
   the root span opens, so worker processes/retries attach spans to the same
   trace; retries become sibling activity spans with `harness.retry` attrs.
4. **Content capture** — off; when `otel.capture_content=true`, prompt/final
   message are attached as span events following semconv content guidance.
5. **Failure isolation** — span emission wrapped so exporter errors increment
   an error counter and log at error level once per interval; never
   propagate into run control flow.
6. **Quickstart** — `docs/otel-trajectory-quickstart.md` + docker-compose
   snippet (Tempo or Langfuse) with a screenshot-level walkthrough.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 span tree shape | lifecycle hooks | integration test with in-memory span exporter asserting hierarchy |
| P3 additive default-off | config gating | tests: counters byte-identical with flag off/on |
| P4 content gating | capture flag | test: content absent without second flag |
| P5 failure isolation | wrapped emitter | test: failing exporter → run completes, error counter increments |

## Data Flow

Runtime state transitions + agent usage parsing → span builder → OTLP
exporter (existing endpoint config) → external backend. Trace ids also
persisted on workflow rows for cross-referencing with internal records.

## Alternatives Considered

- Log-based export (OTLP logs) — rejected: consumers want traces.
- Exporting from Postgres via a sidecar ETL — rejected for v1: doubles the
  data model; direct emission is simpler and real-time.
- Waiting for semconv stability — rejected: mark experimental instead; the
  consuming ecosystem already reads the Development-stability names.

## Risks

- Security: span attributes could leak repo-private strings — attribute set
  is a fixed allowlist; content behind a second flag.
- Compatibility: upstream semconv renames — attributes centralized in one
  module; experimental labeling.
- Performance: span overhead per turn is tiny vs agent runtime; measured in
  the integration test and documented.
- Maintenance: two telemetry surfaces (internal records + spans) — spans are
  derived-only; internal records remain the source of truth.

## Test Plan

- [ ] Unit tests: attribute mapping (model/tokens/cost/outcome), config gating, allowlist.
- [ ] Integration tests: in-memory exporter span-tree shape; retry siblings; failure isolation.
- [ ] Manual verification: quickstart against Tempo or Langfuse with one real issue→PR run.

## Rollback Plan

Set `otel.trajectory=false` (default) — tracer never initializes; metrics
pipeline untouched. No persisted-schema changes beyond a nullable trace-id
column, which can remain.
