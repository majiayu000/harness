# Tech Spec

## Linked Issue

GH-1771

## Product Spec

`specs/GH1771/product.md`

## Current System

### Untrusted-content handling

- `crates/harness-core/src/prompts/issue.rs:6-9` — `wrap_external_data`
  fences content in `<external_data>` and escapes the closing tag. Used for
  issue bodies and prior agent output in `prompts/issue.rs` and
  `prompts/contract.rs`.
- `crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs:24` —
  `REPO_MEMORY_PROMPT_PREAMBLE` frames retrieved repo memory as untrusted
  background evidence.
- `prompt_packet.rs:35-48` — packet construction clones the entire
  `workflow.data` object (`data` clone at `:36`) into the packet `workflow`
  value next to server-owned identity fields; `:73` injects raw
  `command_input` (`job.input`); `:68-71` injects `workflow_file.config`
  and `prompt_template` from the repository; `:111-112` lifts
  `workflow.data.continuation` into the packet. None of these carry trust
  framing.
- `prompt_packet.rs:215` — system framing instructs the agent to treat the
  workflow database as the source of orchestration state.
- `specs/GH1732/` (merged) — packet schema
  `harness.runtime.prompt_packet.v2`: every newly produced packet must
  declare v2 and carry a `context_provenance` manifest with per-entry trust
  levels. Observational — it records what was selected; rendering is
  unchanged by it.

### Agent profiles

- `crates/harness-agents/src/claude.rs:115-131` — `base_args`: full profile
  (`allowed_tools = None`) → `--dangerously-skip-permissions` (pushed at
  `:131`); scoped profile → `--allowedTools`. The flags are mutually
  exclusive.
- `crates/harness-agents/src/claude_adapter.rs:87-93` — same contract for
  the streaming adapter; `claude_adapter.rs:316-320` documents
  auto-approval. Repo rule: both files must change together.
- `workflow_runtime_worker/runtime_profile.rs:27-38` — runtime profiles can
  already declare `sandbox: "read-only-with-network"`. The resolved mode
  flows agent-agnostically: `workflow_runtime_worker/executor.rs:99-101`
  computes it via `runtime_profile_sandbox_mode()` (falling back to the
  `agents.sandbox_mode` config default) and passes it through
  `TurnLifecycleOptions` (`executor.rs:197`;
  `turn_engine/turn_lifecycle.rs:227`, `:243`) into the `AgentRequest` for
  any agent. Claude spawns honor it as a real OS filesystem sandbox —
  `claude_adapter.rs:104-109` and `claude.rs:85`, `:184-189`, `:313-318`
  build `SandboxSpec`s enforced by harness-sandbox Seatbelt/Landlock; codex
  paths translate it to CLI sandbox config (`codex.rs:667-676`,
  `codex_adapter.rs:566-580`). What is missing is narrower: the Claude
  *tool-permission flag surface* is sandbox-unaware
  (`--dangerously-skip-permissions` under the full profile), and no
  per-activity tool profile exists.

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
- `crates/harness-sandbox/src/lib.rs` — Seatbelt policy generation emits
  `(allow network-outbound)` at `lib.rs:154` and protected-path
  `(deny file-write* ...)` rules at `lib.rs:184-188`
  (`PROTECTED_RELATIVE_PATHS` at `lib.rs:11`), giving a host-tier
  enforcement seam.

## Proposed Design

### 1. `workflow.data` provenance sidecar

New module `crates/harness-workflow/src/runtime/data_provenance.rs`:

```rust
pub enum DataProvenance { Server, Agent, External }

pub struct ProvenanceMap {
    entries: BTreeMap<String /* JSON pointer */, DataProvenance>,
    /// When classification began for this instance — the grandfathering
    /// boundary for fields written before the sidecar existed.
    migrated_at: DateTime<Utc>,
}
```

- Stored in `workflow_instances` as a sibling JSONB column
  `data_provenance` (migration in `runtime/store_migrations.rs`), written
  in the same transaction as every `data` mutation
  (`runtime/store/transaction_helpers.rs` gains
  `write_data_with_provenance`; existing helpers delegate with an explicit
  class).
- Writer classification:
  - reducers writing snapshot-derived facts (`github_pr_snapshot`
    consumers, reconciliation, binding metadata) → `Server`;
  - any value parsed from an activity result
    (`workflow_runtime_worker/activity_result.rs` consumers, continuation
    writes, `summary`, `last_external_state`) → `Agent`;
  - webhook/issue/comment text stored by intake → `External`.
- No backfill. Fields with no entry are grandfathered by `migrated_at`:
  they render fenced-as-untrusted with a degradation artifact (product
  B-004). A missing entry for a field written *after* `migrated_at` is a
  writer defect → typed construction error. Operator recovery may stamp a
  field `Agent` (conservative), never `Server`.

### 2. Packet schema v3: fenced rendering

GH1732 makes v2 mandatory for every newly produced packet, so fencing
obligations attach to a `harness.runtime.prompt_packet.v3` bump (one
shared schema constant, superset of v2):

- `build_prompt_packet` loads the provenance map with the instance (same
  row, one query).
- v3 rendering splits `workflow.data`:
  - `Server` fields render in place, byte-identical to v2;
  - `Agent`/`External`/grandfathered fields move to a
    `workflow.untrusted_data` object under a preamble in the
    `REPO_MEMORY_PROMPT_PREAMBLE` contract family; string leaves pass
    through `wrap_external_data`;
  - `continuation_context` always renders in the untrusted section.
- Typed error `PromptPacketError::UnclassifiedField { pointer }` for
  post-sidecar unclassified fields (surfaces as a Configuration-kind
  activity failure, retryable after operator stamping).
- The GH1732 provenance manifest gains one entry kind for the untrusted
  section so packet evidence and rendering agree; historical v1/v2 packets
  are never reinterpreted.

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
  runtime call sites pass a new `SpawnPermissionMode` enum instead of the
  `Option<Vec<String>>` sentinel, so full permissions can never be
  inferred from absence. `claude.rs` and `claude_adapter.rs` change
  together.
- Effective profile recorded into packet evidence and OTel span
  attributes.

### 4. Egress floor

- Host tier: `SandboxSpec` gains
  `egress: EgressPolicy { Deny | Allowlist(Vec<HostPattern>) | Open }`.
  macOS Seatbelt generation replaces the unconditional
  `(allow network-outbound)` with policy-derived rules; Linux uses bwrap
  netns (`--unshare-net`) plus a slirp/proxy helper when an allowlist is
  configured. A configured policy the platform cannot honor is a typed
  dispatch error (product B-007).
- Container tier: `docker/egress-proxy/` bundles a minimal filtering proxy
  image; `spawn_contract.rs` gains a pre-dispatch canary — one allowlisted
  request must succeed and one non-allowlisted request must be refused —
  before the agent container starts (product B-008). The `none` fallback
  is preserved verbatim.

## Data Flow

1. Intake/webhook stores external text → `write_data_with_provenance`
   stamps `External`.
2. Reducer consumes an activity result → agent-derived fields stamped
   `Agent`; snapshot-derived facts stamped `Server` — same transaction as
   the `data` mutation.
3. Worker claims a job → `build_prompt_packet` loads instance + provenance
   map → v3 renderer partitions fields (server in place; agent/external/
   grandfathered into fenced `workflow.untrusted_data`; continuation always
   fenced) → unclassified post-sidecar field aborts with
   `UnclassifiedField`.
4. Activity policy resolves the tool profile → spawn contract emits
   `--allowedTools` (or opted-up skip-permissions) plus the egress policy
   → container canary / host sandbox rules applied → agent starts.
5. Effective profile, egress mode, fencing counts, and degradation
   artifacts land in runtime evidence and OTel attributes.

## Product-to-Test Mapping

| Behavior | Test surface |
| --- | --- |
| B-001 provenance recorded per writer | `harness-workflow` `runtime::data_provenance` unit tests + reducer tests per write path (snapshot→Server, activity-result→Agent, intake→External) |
| B-002 v3 fencing, server byte-identical, historical packets untouched | `harness-server` `workflow_runtime_worker::prompt_packet` snapshot tests: v2 vs v3 fixture diff; v1/v2 fixture readback |
| B-003 continuation always fenced | prompt_packet test with continuation present/absent |
| B-004 grandfather vs writer-defect split | prompt_packet tests: pre-`migrated_at` field → fenced + degradation artifact; post-`migrated_at` unclassified → `UnclassifiedField` error |
| B-005 / B-006 scoped default, explicit opt-up, flag exclusivity | `cargo test --package harness-agents` spawn-arg tests for both adapters; config resolution tests for `SpawnPermissionMode` |
| B-007 host egress deny-by-default + typed platform error | `harness-sandbox` policy-generation tests (Seatbelt/Landlock rule sets per `EgressPolicy`); dispatch error test |
| B-008 container canary + preserved `none` fallback | `spawn_contract` tests: canary pass/fail gating; `container_network_mode` regression |
| B-009 evidence completeness | runtime evidence tests asserting profile, egress mode, fencing counts per job |
| B-010 compatibility | end-to-end runtime test: in-flight v2 workflow retried post-ship produces v3 packet, unblocked, degradation artifacts recorded |

## Alternatives Considered

- **Per-definition fencing flag independent of schema version** — rejected:
  declared schema and rendering behavior could diverge, breaking GH1732's
  discipline that the packet schema states which obligations apply, and
  evidence consumers would need a second signal to interpret a packet.
- **Timestamp-only grandfathering without a fail-closed tier** — rejected:
  a post-sidecar writer bug would silently render new agent-written fields
  as trusted; the two-tier split keeps legacy data flowing while making
  new classification gaps loud.
- **Inline provenance annotations inside `workflow.data`** — rejected:
  mutates shapes existing consumers read; the sidecar keeps `data`
  byte-compatible.
- **Backfilling provenance for existing rows** — rejected: classification
  requires writer context that no longer exists; conservative fencing of
  legacy fields is safe and self-corrects as fields are rewritten.
- **Treating repo-sourced `prompt_template` as untrusted** — out of scope
  by product Non-Goals: it is instruction-bearing by design; control is
  repo write access plus provenance visibility.

## Risks

- Fencing changes prompt shape for v3 packets; candidate/eval baselines
  that byte-compare packets must re-baseline (schema-versioned, bounded).
- Over-classification (`Agent` stamped on server-derived data) degrades
  gracefully — content present, just fenced; under-classification is the
  dangerous direction and is blocked by the fail-closed tier plus
  writer-side tests.
- Host egress on Linux is the highest-effort item; it ships last and its
  absence blocks only configurations that explicitly demand enforcement.
- Scoped-default flip can break operator setups that relied on implicit
  full permissions; mitigated by release-note callout and explicit opt-up.

## Test Plan

- `cargo test --package harness-workflow runtime::data_provenance`
- `cargo test --package harness-server workflow_runtime_worker::prompt_packet`
- `cargo test --package harness-agents` (spawn arg contracts, both
  adapters, `SpawnPermissionMode`)
- `cargo test --package harness-sandbox` (policy generation per
  `EgressPolicy`)
- Full gates before push: `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo fmt --all -- --check`.

## Rollback Plan

Each step is independently revertible:

1. Provenance sidecar (dark): revert the migration consumer; the JSONB
   column and accumulated classifications are inert.
2. v3 fencing: pin the packet schema constant back to v2 — rendering
   reverts wholesale with no data migration; the sidecar keeps
   accumulating classifications for a later re-flip. Historical packets
   are untouched either way.
3. Scoped default: configuration flip back to the previous default
   profile; no code revert needed.
4. Egress: disable per-tier enforcement config; container tier returns to
   the existing allowlist+proxy env-export behavior, host tier to
   unconditional `(allow network-outbound)`.
