# Tech Spec

## Linked Issue

GH-1771

## Current State

### Untrusted-content handling

- `crates/harness-core/src/prompts/issue.rs:6-9` — `wrap_external_data`
  fences content in `<external_data>` and escapes the closing tag. Used for
  issue bodies and prior agent output in `prompts/issue.rs` and
  `prompts/contract.rs`.
- `crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs:24` —
  `REPO_MEMORY_PROMPT_PREAMBLE` frames retrieved repo memory as untrusted
  background evidence.
- `prompt_packet.rs:35-45` — packet construction clones the entire
  `workflow.data` object into the packet `workflow` value next to
  server-owned identity fields; `prompt_packet.rs:73` injects raw
  `command_input` (`job.input`); `prompt_packet.rs:68-71` injects
  `workflow_file.config` and `prompt_template` from the repository;
  `prompt_packet.rs:111-112` lifts `workflow.data.continuation` into the
  packet. None of these carry trust framing.
- `prompt_packet.rs:213-215` — system framing instructs the agent to treat
  the workflow database as the source of orchestration state.
- `specs/GH1732/` (packet schema `harness.runtime.prompt_packet.v2`) —
  records a provenance manifest for packet *sources* with per-entry trust
  levels. Observational; rendering is unchanged by it.

### Agent profiles

- `crates/harness-agents/src/claude.rs:115-131` — `base_args`: full profile
  (`allowed_tools = None`) → `--dangerously-skip-permissions`; scoped
  profile → `--allowedTools`. The flags are mutually exclusive.
- `crates/harness-agents/src/claude_adapter.rs:87-93` — same contract for
  the streaming adapter; `claude_adapter.rs:316-320` documents auto-approval.
- Both spawn paths must change together (repo rule: adapter arg
  construction is duplicated across `claude.rs` and `claude_adapter.rs`).
- The workflow runtime does not select a profile per activity today.

### Egress

- `crates/harness-agents/src/spawn_contract.rs:16-17` —
  `HARNESS_AGENT_EGRESS_PROXY` / `HARNESS_AGENT_EGRESS_ALLOWLIST` env
  constants.
- `spawn_contract.rs:249-258` — `container_network_mode` returns `none`
  unless allowlist **and** proxy are both configured, else `bridge`.
- `spawn_contract.rs:96-107` — allowlist exported as an env var for an
  external proxy; proxy URL exported as `HTTP_PROXY`/`HTTPS_PROXY`/
  `ALL_PROXY`. No proxy is bundled in `docker/`; nothing verifies the proxy
  filters anything. Host tier has no network control.
- `crates/harness-sandbox/src/lib.rs` — Seatbelt/Landlock/bwrap generation
  exists and already carries per-spec network toggles (`--network` handling
  for bwrap at `lib.rs:220`), giving a host-tier enforcement seam.

## Design

### 1. `workflow.data` provenance sidecar

New module `crates/harness-workflow/src/runtime/data_provenance.rs`:

```rust
pub enum DataProvenance { Server, Agent, External }

pub struct ProvenanceMap(BTreeMap<String /* JSON pointer */, DataProvenance>);
```

- Stored in `workflow_instances` as a sibling JSONB column
  `data_provenance` (migration in `runtime/store_migrations.rs`), written in
  the same transaction as every `data` mutation
  (`runtime/store/transaction_helpers.rs` gains a
  `write_data_with_provenance` helper; existing helpers delegate with an
  explicit class).
- Writer classification:
  - reducers writing snapshot-derived facts (`github_pr_snapshot`
    consumers, reconciliation, binding metadata) → `Server`;
  - any value parsed from an activity result
    (`workflow_runtime_worker/activity_result.rs` consumers, continuation
    writes, `summary`, `last_external_state`) → `Agent`;
  - webhook/issue/comment text stored by intake → `External`.
- Unclassified legacy fields: a backfill pass is **not** attempted.
  Instead, packet-v2 construction treats missing provenance as an error
  (B-004); v1 packets ignore the sidecar entirely. Recovery for stuck v2
  workflows: an operator recovery action may stamp a field `Agent`
  (conservative), never `Server`.

### 2. Fenced rendering in packet construction

`prompt_packet.rs`:

- `build_prompt_packet` gains the provenance map (loaded with the instance;
  one query, same row).
- For v2+ schemas, the packet `workflow.data` is split at render time:
  - `Server` fields render in place (byte-identical to today);
  - `Agent`/`External` fields move to a `workflow.untrusted_data` object
    rendered under a preamble with the same contract text family as
    `REPO_MEMORY_PROMPT_PREAMBLE`; string leaves additionally pass through
    `wrap_external_data`.
  - `continuation_context` always renders in the untrusted section.
- Typed error `PromptPacketError::UnclassifiedField { pointer }` fails
  packet construction (surfaces as a Configuration-kind activity failure,
  retryable after operator stamping).
- The GH1732 provenance manifest gains one entry kind for the untrusted
  section so packet evidence and rendering agree.

### 3. Scoped default profile

- `crates/harness-core/src/config/` gains per-activity tool profiles:

```toml
[agents.claude.profiles.read]   # triage, inspect, review
allowed_tools = ["Read", "Grep", "Glob"]
[agents.claude.profiles.write]  # implement, address feedback
allowed_tools = ["Read", "Grep", "Glob", "Edit", "Write", "Bash(git:*)", "Bash(cargo:*)", "Bash(gh:*)"]
[agents.claude.profiles.full]
dangerously_skip_permissions = true   # explicit opt-up only
```

- `workflow_runtime_worker` activity policy maps activity → profile name;
  spawn paths receive a resolved `allowed_tools` and never infer full from
  `None` in runtime context (a new `SpawnPermissionMode` enum replaces the
  `Option` sentinel at the runtime call sites; `claude.rs` and
  `claude_adapter.rs` change together, verified by
  `cargo test --package harness-agents`).
- Effective profile recorded into the packet evidence and OTel span
  attributes.

### 4. Egress floor

- Host tier: `SandboxSpec` gains `egress: EgressPolicy { mode: Deny |
  Allowlist(Vec<HostPattern>) | Open }`. macOS Seatbelt generation emits
  network deny/allow rules alongside the existing file rules; Linux uses
  bwrap netns (`--unshare-net`) plus a slirp/proxy helper when an
  allowlist is configured. Platforms that cannot honor a configured policy
  return a typed dispatch error (B-007).
- Container tier: `docker/egress-proxy/` bundles a minimal filtering proxy
  image; `spawn_contract.rs` gains a pre-dispatch canary — one allowlisted
  request must succeed and one non-allowlisted request must be refused —
  before the agent container starts (B-008). Existing `none` fallback
  preserved verbatim.

## Migration Order

1. Provenance sidecar: migration + writer classification + tests (dark;
   no rendering change).
2. Packet v2 fenced rendering behind the schema version; regression test
   for the two-turn replay attack.
3. Profile resolution + spawn changes (`claude.rs` +
   `claude_adapter.rs` together); config default flips with release notes.
4. Egress: container canary + bundled proxy first, host-tier Seatbelt
   rules second, Linux netns last.

Each step is independently revertible; step 2 reverts by pinning
definitions back to v1 packets.

## Validation

- `cargo test --package harness-workflow runtime::data_provenance`
- `cargo test --package harness-server workflow_runtime_worker::prompt_packet`
- `cargo test --package harness-agents` (spawn arg contracts, both adapters)
- `cargo test --package harness-sandbox` (policy generation)
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --all -- --check` before push.

## Risks

- Fencing changes prompt shape for v2 workflows; candidate/eval baselines
  that byte-compare packets must re-baseline (schema-versioned, so bounded).
- Over-classification (`Agent` stamped on genuinely server-derived data)
  degrades prompt authority gracefully — content still present, just
  fenced; under-classification is the dangerous direction and is prevented
  by the fail-closed default plus writer-side tests.
- Host egress on Linux is the highest-effort item; it ships last and its
  absence blocks only configurations that explicitly demand enforcement.
