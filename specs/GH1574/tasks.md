# GH1574 Task Plan

## Linked Issue

GH-1574

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1574-T001` Owner: `core-contract` | Done when: `ObservationCompressor` trait, `Compressed`, `NapStatus`, `CompressHint`, and `CompressError` land in `harness-core` with docs | Verify: `cargo check -p harness-core`
- [ ] `SP1574-T002` Owner: `compressor` | Done when: `PromptCompressor` compresses fixtures via a mocked model client, honors `min_size_bytes`, and is inert without config | Verify: `cargo test -p harness-context compress`
- [ ] `SP1574-T003` Owner: `nap-verify` | Done when: sampled NAP verification compares `{intent, target_files, command_class}` sketches, falls back to raw on mismatch, and auto-disables past 15% failure with an error-level event | Verify: `cargo test -p harness-context nap`
- [ ] `SP1574-T004` Owner: `composer-seam` | Done when: `Degraded::Summarized { nap }` exists and `ComposeManifest` items carry optional token/nap fields without breaking existing manifest consumers | Verify: `cargo test -p harness-context`
- [ ] `SP1574-T005` Owner: `agent-seam` | Done when: subagent final output passes through the compressor when enabled, with raw artifact path recorded before replacement | Verify: `cargo test -p harness-agents`
- [ ] `SP1574-T006` Owner: `replay-eval` | Done when: an A/B replay script reports token saving, NAP mismatch rate, and cost ratio over the 40-session measurement set | Verify: script output attached to the PR; gate numbers from product.md

## Parallelization

- `SP1574-T001` first; `T002`/`T003` share the compressor module and stay
  serial; `T004` and `T005` are disjoint crates and can run in parallel
  after `T001`.
- `SP1574-T006` runs last against a built binary.

## Verification

- `cargo check --workspace`
- `cargo test -p harness-core -p harness-context -p harness-agents`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1574`

## Handoff Notes

Feature ships disabled by default. If the Phase 1 gate fails, keep the
trait and tests, remove the seams from default builds, and record the
downgrade decision in this packet.
