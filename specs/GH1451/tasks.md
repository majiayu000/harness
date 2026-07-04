# Task Plan

## Linked Issue

GH-1451

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1451-T001` Owner: observe | Done when: `otel.trajectory` / `otel.capture_content` flags exist and `OtelPipeline` wires a TracerProvider alongside the existing MeterProvider, default off | Verify: `cargo test -p harness-observe otel_trajectory_config`
- [ ] `SP1451-T002` Owner: observe | Done when: the attribute mapping module covers GenAI semconv names (model, input/output tokens) plus the harness namespace as a single allowlist, omitting unknown values instead of zero-filling | Verify: `cargo test -p harness-observe otel_attributes`
- [ ] `SP1451-T003` Owner: workflow-runtime | Done when: workflow/activity lifecycle hooks emit the span tree, trace ids persist on workflow rows, and retries appear as sibling spans with retry attributes | Verify: `cargo test -p harness-workflow otel_spans`
- [ ] `SP1451-T004` Owner: agents | Done when: agent-turn spans carry token/model/cost attributes fed from usage parsing | Verify: `cargo test -p harness-agents otel_turn_spans`
- [ ] `SP1451-T005` Owner: observe | Done when: exporter failures drop spans, increment an error counter, and never affect run control flow | Verify: `cargo test -p harness-observe otel_failure_isolation`
- [ ] `SP1451-T006` Owner: docs | Done when: `docs/otel-trajectory-quickstart.md` plus a compose snippet render a real run's trace in Tempo or Langfuse | Verify: manual quickstart walkthrough recorded in the PR

## Parallelization

- T001 ∥ T002 (disjoint: otel_export.rs vs new mapping module).
- T003 after T001; T004 after T002; T005 with T003/T004 integration; T006 last.

## Verification

- `cargo test --workspace`
- Integration: in-memory exporter asserts tree shape, retry siblings, content gating, and counter parity with the flag off.
- Manual: quickstart renders a real issue→PR run's trace in the chosen backend.

## Handoff Notes

- Internal runtime records remain the source of truth; spans are derived and droppable.
- Every exported attribute lives in the single allowlist module — that module is the leak-review surface.
