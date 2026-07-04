# Product Spec

## Linked Issue

GH-1451

## User Problem

Operators cannot inspect a run as a step-level trace in standard tooling.
Harness exports custom OTLP counters only; trajectory data (which activity,
which agent turn, how many tokens, what cost, what outcome) lives in bespoke
Postgres records invisible to Grafana/Langfuse/Phoenix-class tools that teams
already run.

## Goals

- Emit an OTel span tree per workflow run using GenAI semantic conventions.
- Keep existing custom counters intact (additive change).
- Make traces consumable by at least one mainstream backend out of the box.

## Non-Goals

- Replacing the internal event store or run evidence model.
- A bundled tracing UI.
- Attribute stability guarantees while upstream GenAI semconv is pre-stable.
- Capturing prompt/response content by default.

## Behavior Invariants

1. With `otel.trajectory` enabled, each workflow run emits one root span with
   child spans per activity and per agent turn, linked by trace context.
2. Spans carry GenAI semconv attribute names for model id, input/output
   tokens, and cost, plus harness identifiers (workflow id, activity kind,
   outcome) under a harness namespace.
3. Trajectory export defaults OFF; enabling it changes no other behavior and
   existing counters keep their names and semantics.
4. Prompt/response content is never exported unless a second, separate
   content-capture flag is enabled; that flag's docs state the privacy
   implications.
5. Export failures degrade to dropped spans with an error-level log counter —
   they never block or fail workflow execution.
6. Span emission overhead is measured and documented; the feature is marked
   experimental while upstream conventions are pre-stable.

## Acceptance Criteria

- [ ] One issue→PR run renders as a linked span tree in an OTLP backend with
      model/token/cost/outcome attributes.
- [ ] Off by default; counters unchanged in both states.
- [ ] Content capture requires the explicit second flag.
- [ ] Quickstart doc shows the trace in Grafana Tempo or Langfuse.

## Edge Cases

- Backend unreachable — spans drop with visible error counter; run unaffected.
- Token/cost unknown for a turn (adapter gap) — attribute omitted, never
  zero-filled (no fake data).
- Very long runs — span batch limits respected; truncation surfaced as a
  span event, not silent loss.
- Trace context across retries — retried activities appear as sibling spans
  with retry attributes, not overwritten.

## Rollout Notes

Experimental flag in config; document attribute names and their upstream
semconv version (v1.41-era, "Development" stability) so consumers know
renames may follow upstream.
