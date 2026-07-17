# GH1574 Task Plan

## Linked Issue

GH-1574

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [x] `SP1574-T001` Owner: `core-contract` | Done when: `ObservationCompressor` trait, `Compressed`, `NapStatus`, `CompressHint`, and `CompressError` land in `harness-core` with docs | Verify: `cargo check -p harness-core`
- [x] `SP1574-T002` Owner: `compressor` | Done when: `PromptCompressor` compresses fixtures via a mocked model client, honors `min_size_bytes`, and is inert without config | Verify: `cargo test -p harness-context compress`
- [x] `SP1574-T003` Owner: `nap-verify` | Done when: sampled NAP verification compares `{intent, target_files, command_class}` sketches, falls back to raw on mismatch, and auto-disables past 15% failure with an error-level event | Verify: `cargo test -p harness-context nap`
- [x] `SP1574-T004` Owner: `composer-seam` | Done when: `Degraded::Summarized { nap }` exists and `ComposeManifest` items carry optional token/nap fields without breaking existing manifest consumers | Verify: `cargo test -p harness-context`
- [x] `SP1574-T005` Owner: `agent-seam` | Done when: subagent final output passes through the compressor when enabled, with raw artifact path recorded before replacement | Verify: `cargo test -p harness-agents`
- [x] `SP1574-T006` Owner: `replay-eval` | Done when: an A/B replay script reports token saving, NAP mismatch rate, and cost ratio over the 40-session measurement set | Verify: `phase1-replay-report.json`; gate numbers from product.md

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

T005 status: ApiCompressModel + builder land in harness-agents; the seam is
wired at plan_output -> implement prompt (implement_pipeline). Remaining
seams for a follow-up: triage->plan (needs a CompressionConfig param through
run_triage_plan_pipeline) and cross_review primary->challenger. The
implement_pipeline.rs edit carries a user-approved U-16 exemption
(file predates the 800-line ceiling); the split is tracked in a separate
issue.


Feature ships disabled by default. If the Phase 1 gate fails, keep the
trait and tests, remove the seams from default builds, and record the
downgrade decision in this packet.

T006 result (2026-07-17): `scripts/run_nap_replay.py` executed the A/B
replay through an agent-runtime channel (default `codex exec` with
`gpt-5.6-luna` on a subscription seat; `claude -p` and an OpenAI-compatible
API are optional channels), mirroring `PromptCompressor` semantics
(prompts, seeded sampling, 2-of-3 sketch agreement, breaker, byte/4
estimates) and binding evidence to the manifest via per-session source
HMAC re-verification. Runs are resumable through per-session state files.
Success oracle is a documented proxy pending a human-approved task oracle:
`baseline_success` is true (sessions completed historically) and
`candidate_success` is false only when the session trips the NAP breaker.
Pricing enters via explicit `--pricing-*` flags; subscription-seat runs may
pass 0 for compressor cost, which trivially satisfies the cost-ratio gate.

The complete 40-session report is `phase1-replay-report.json`, bound to
manifest `56ea095bfcc140faea8ead3ef862af08864c3db8b83292393a2e3031bd69d40c`.
The Phase 1 gate failed: effective token saving was 4.23% (minimum 20%), NAP
mismatch was 87.77% (maximum below 5%), and the proxy recorded 36 paired
success regressions (maximum 0). The subscription-seat compressor cost was
0 USD, so the 0% cost ratio passed trivially and is not evidence of product
viability. Per the approved downgrade rule, compression remains disabled by
default; the trait and tests remain, and Phase 1 does not advance.
