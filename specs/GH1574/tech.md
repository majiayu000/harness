# GH1574 Tech Spec: NAP-Lite Behavior-Preserving Observation Compression

Product spec: `specs/GH1574/product.md`
GitHub issue: `#1574`

## Context

- `harness-context` composes pre-run context via `ContextComposer` with
  quotas (`ContextQuotas`), a `Degraded` state, and a `ComposeManifest`
  that already records per-item decisions.
- `harness-agents` adapters (`claude_adapter`, `codex_adapter`,
  `codex_exec_parser`) parse agent event streams and deliver subagent
  results to the orchestrator.
- The inner tool loop of Claude Code / Codex is not interceptable; the only
  seams Harness owns are (A) subagent result ingestion and (B) the
  over-budget degradation path in the composer.

## Proposed Design

### Contract (harness-core)

```rust
pub trait ObservationCompressor: Send + Sync {
    fn compress(&self, obs: &str, hint: &CompressHint)
        -> Result<Compressed, CompressError>;
}

pub struct CompressHint {
    pub task_summary: String,     // short task framing for the compressor prompt
    pub source: ObsSource,        // AgentResult | ComposeDegradation
}

pub struct Compressed {
    pub text: String,
    pub original_tokens: usize,
    pub compressed_tokens: usize,
    pub nap: NapStatus,
}

pub enum NapStatus {
    Verified,                     // sampled and passed
    SkippedSample,                // not in the verification sample
    Failed { fell_back: bool },   // mismatch; fell_back must be true
}
```

### Implementation (new module, e.g. `harness-context/src/compress.rs`)

- `PromptCompressor`: calls a small model through the existing provider
  plumbing in `harness-agents`/`anthropic_api` or an OpenAI-compatible
  endpoint; model id and endpoint come from config only — no default model,
  unconfigured means the compressor is inert.
- Inputs below `min_size_bytes` return `CompressError::TooSmall` and the
  caller keeps raw text (not an error path, a skip path).
- NAP verification: with probability `sample_rate`, issue two sketch
  requests (raw, compressed) asking for JSON
  `{intent, target_files, command_class}`; compare fields; >= 2/3 agree
  passes. Sketch requests use the same small model.
- Per-task failure tracking: a counter on the compressor handle; when
  `nap_failed / nap_checked > 0.15` (min 5 checks), set the task to
  bypass mode and emit an error-level event through the existing
  observability channel (`harness-observe`).

### Seam A: subagent result ingestion (harness-agents)

Where the orchestrator accepts a subagent final output, pass it through the
compressor when enabled. Store both the compressed text (into parent
context) and a pointer to the raw artifact (existing artifact/log paths) so
nothing is lost.

### Seam B: composer degradation (harness-context)

Extend `Degraded` with `Summarized { nap: NapStatus }`. When an item
exceeds its quota and compression is enabled, try summary-compression
before truncation. `ComposeManifest` items gain optional
`original_tokens`, `compressed_tokens`, `nap` fields (serde default, no
schema break for existing manifests).

### Config

```toml
[context.compression]
enabled = false
model = ""            # required to activate
endpoint = ""         # optional; defaults to provider plumbing
sample_rate = 0.10
min_size_bytes = 2048
```

## Boundaries

- No compression of rules, skills, contracts, or exec-plan items — only
  observation-class content (agent results, tool-output artifacts).
- Raw text is always recoverable (artifact path recorded before replace).
- Failure semantics follow no-silent-degradation: fallback is explicit in
  the manifest and error-level when systemic.

## Verification

- Unit fixtures: a cargo-test log, a `gh api` JSON blob, a build log —
  compression + NAP compare with a mocked model client (no network in CI).
- Fallback-path test: forced NAP mismatch must produce raw text,
  `Failed { fell_back: true }`, and the counter increment.
- Auto-disable test: >15% failure over >=5 checks flips bypass and emits
  the error event.
- Replay eval (out of CI): A/B over the 40-session measurement set;
  gate numbers in product.md.
