# Tech Spec

## Linked Issue

GH-1732

## Product Spec

See `specs/GH1732/product.md`.

<!-- specrail-planned-changes
{"issue":1732,"complete":true,"paths":["crates/harness-core/src/config/workflow.rs","crates/harness-server/src/workflow_runtime_worker/executor.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet_activity_policy_tests.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet_tests.rs","crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012","B-013","B-014"]}
-->

## Current System

- `crates/harness-server/src/workflow_runtime_worker/prompt_packet.rs:26-97`
  builds one JSON prompt packet from runtime job, workflow instance, project
  roots, runtime profile, effective workflow document, selected repo memory,
  activity policy, and continuation context.
- `prompt_packet.rs:49-87` serializes runtime profile, workflow file content,
  command input, runtime contract, result schema, and structured-output
  requirements without source-level provenance.
- `prompt_packet.rs:88-90` includes selected repo-memory representations only
  when retrieval returned records.
- `prompt_packet.rs:756-769` hashes the complete packet and emits the existing
  compact `runtime_prompt_packet` activity artifact.
- `crates/harness-server/src/workflow_runtime_worker/executor.rs:124-159`
  retrieves repo memory, builds the packet, hashes it, records prompt
  preparation, and only then constructs and executes the agent prompt.
- `executor.rs:298-317` persists `RuntimePromptPrepared` with the packet digest
  and complete packet in one runtime event.
- `executor.rs:99-147` resolves sandbox, model, reasoning effort, and approval
  separately from the `RuntimeProfile` serialized into the packet. It also
  resolves durable prompt-task text before packet construction but appends
  that text only after the packet has already been hashed and recorded.
- `crates/harness-server/src/workflow_runtime_worker/repo_memory_prompt.rs:16-68`
  returns either selected records or explicit retrieval-degradation evidence;
  it does not fabricate records on failure.
- `crates/harness-workflow/src/runtime/memory_retrieval.rs:26-30` gives selected
  memory its durable record and estimated token count.
- `crates/harness-workflow/src/runtime/model.rs:469-484` defines declared
  runtime-profile fields, some of which may be absent and resolved from server
  or workflow defaults at launch.
- `crates/harness-core/src/config/workflow.rs:15-23` defines effective workflow
  config, prompt template, and one display-oriented `source_path`. The loader
  can combine central and repository files into a string, so this field cannot
  truthfully preserve each source by itself.
- `prompt_packet.rs` is currently 794 lines, so new provenance logic cannot be
  added inline without violating the repository's 800-line ceiling.

## Proposed Design

### Focused Provenance Module

Add `prompt_packet/context_provenance.rs` and declare it as a private submodule
from `prompt_packet.rs`. The module owns:

- `CONTEXT_PROVENANCE_SCHEMA`;
- construction of `ContextProvenance`;
- normalized digest helpers for runtime profile, workflow document, and memory;
- prompt-task reference/text binding without raw-text duplication;
- safe source-locator normalization;
- coverage declarations for context not observed by Harness.

Use ASC-001 types from `harness_core::stack`; do not copy component, source,
trust, freshness, capability, or selection enums into the server crate.
Provenance has a private serializable envelope containing the schema,
ordered validated `AgentStackComponent` entries, and a closed list of coverage
limitations.

This is an internal domain boundary, not a `harness-protocol` response
contract. `harness_core::stack` is intentionally the canonical cross-crate
Agent Stack model established by ASC-001, so protocol-local duplicate newtypes
would weaken that contract. `build_runtime_prompt_packet` continues to return
its existing private `serde_json::Value`; this issue adds no response DTO or
external consumer contract.

### Shared Resolved Runtime Settings

Add private `ResolvedRuntimeSettings` in `runtime_profile.rs`. One resolver
accepts the already timeout-adjusted `RuntimeProfile`, `RuntimeKind`, and server
agent configuration and returns the final profile name, kind, model, reasoning
effort, sandbox, approval policy, max-turns, and timeout. `executor.rs` creates
this value once, passes it to packet/provenance construction, and uses the same
launch fields for `TurnLifecycleOptions` and agent request construction.
`max_turns` is copied from the effective profile already enforced by the
workflow runtime; it is evidence, not a second enforcement point. Existing
validation helpers become implementation details of this single resolver.

The packet retains the declared/effective `runtime_profile` section for
compatibility and adds `resolved_runtime_settings`. Provenance hashes canonical
JSON of only the resolved value. No launch field is recomputed after the
packet is recorded.

### Workflow Source Preservation

Extend `WorkflowDocument` with an observation-only, serde-skipped ordered
source list. Each source records whether it is the central base or repository
override, its original path for in-process classification, and SHA-256 of the
exact file bytes read. `load_workflow_document_with_base` retains central then
repository order while preserving the existing merged config, prompt template,
and compatibility `source_path`.

The provenance builder emits:

- one runtime-scoped central-source entry whose locator contains a digest of
  the canonical path rather than the absolute path;
- one repository-scoped `WORKFLOW.md` entry when the repository file exists;
- one runtime-scoped effective-document entry whose digest covers normalized
  merged config and prompt template; or
- one runtime-scoped defaults entry when no configured source exists.

Configured-versus-default classification comes from the source list, never
from whether the display path can be normalized relative to the project.

`prompt_packet.rs` calls the module once after creating the base packet fields
and before activity policy or final hashing:

1. build provenance from `runtime_profile`, `workflow_document`, selected
   `repo_memory`, resolved runtime settings, and the already constructed packet
   sections;
2. serialize it through `serde_json::to_value` with contextual error handling;
3. assign `packet["context_provenance"]`;
4. continue existing activity-policy and continuation processing.

Any validation or serialization error propagates from
`build_runtime_prompt_packet`. The caller therefore does not hash, record, or
execute an incomplete packet.

### Source Entries

Resolved runtime settings:

- kind `agent_runtime`;
- ID `runtime-profile:<profile name>`;
- runtime-scoped locator using the profile name;
- digest of canonical serde JSON for `ResolvedRuntimeSettings`;
- reason `workflow_runtime_profile_selected`;
- observation/trust `runtime_observed`;
- selection `loaded`;
- order 0.

Workflow sources and effective document:

- central and repository source entries use the ordered source facts retained
  by `WorkflowDocument`;
- source digests cover exact bytes read, while the effective-document digest
  covers canonical JSON containing merged `config` and `prompt_template`;
- central absolute paths are represented only by a runtime-scoped path digest;
- the repository source uses the normalized `WORKFLOW.md` locator;
- reasons distinguish `workflow_base_selected`,
  `workflow_repository_selected`, `workflow_document_effective`, and
  `workflow_defaults_selected`;
- observation/trust are `runtime_observed`, selection is `loaded`, and orders
  follow the runtime entry then central source, repository source, effective
  document, and memory records.

Repo memory:

- kind `memory`;
- ID and runtime locator derived from the durable memory record ID;
- digest of the exact redacted JSON representation returned by
  `repo_memory_prompt_value`, scoped to that record;
- safe metadata fields for durable ID, optional evidence reference, and
  estimated token count stored in the provenance entry's typed metadata
  extension defined by this module, not in the ASC-001 component;
- reason `repo_memory_selected`;
- observation/trust `runtime_observed`;
- selection `loaded`;
- order immediately after the final workflow entry, preserving memory selection
  order.

The module never copies raw memory payload into provenance. The payload remains
only in the existing packet memory section. On retrieval failure, the input
record list is empty and no memory entry is emitted; the existing degradation
artifact remains the failure evidence.

### Dynamic Payload and Coverage

Do not create reusable context components for workflow instance, command input,
prompt-task request, activity result schema, or continuation payload. They are
per-invocation packet sections and remain covered by the enclosing packet
digest.

Pass `prompt_task_request.prompt_text()` into `build_runtime_prompt_packet`.
When present, add a redacted `prompt_task_request` packet section containing
the existing non-empty `prompt_ref` and lowercase SHA-256 of the exact UTF-8
task text. Do not place raw task text in the packet or provenance. The existing
`build_runtime_job_prompt` appends that same text after packet rendering, so
prompt semantics remain unchanged while the recorded packet digest becomes
sensitive to the text actually executed.

Add closed coverage markers for:

- `agent_cli_context_not_observed`;
- `mcp_host_context_not_observed`;
- `user_global_context_not_observed`;
- `model_provider_context_not_observed`.

These markers prevent consumers from treating absence as a proof. They contain
no guessed file paths or values.

### Deterministic Hashing

Reuse `serde_json::to_vec` plus SHA-256 for individual normalized sources.
Source structs use deterministic field order and no unordered maps. Repo-memory
entry order is the existing retrieval order. The complete packet digest
continues to cover provenance, activity policy, continuation context, and all
dynamic payloads.

Do not reuse the current `prompt_packet_digest` fail-soft empty-vector fallback
for source digests. Provenance serialization is required evidence and returns
an error on failure. A later implementation may harden the existing packet
helper separately; this issue does not silently change its public behavior.

### Test Layout

Keep new tests in
`prompt_packet/context_provenance_tests.rs` and include them from the provenance
module. Existing `prompt_packet_tests.rs` assertions remain unchanged except
where full packet fixtures must acknowledge the additive field.

Tests construct real `RuntimeProfile`, `WorkflowDocument`, and
`RetrievedRepoMemoryRecord` values, then assert complete provenance entries,
absence of duplicated payloads, coverage markers, stable ordering, source
digest sensitivity, and packet digest sensitivity.

Keep `prompt_packet.rs` at or below the repository's 800-line hard ceiling by
placing construction, hashing, and prompt-task binding helpers in the focused
submodule; the parent module contains only the minimal call sites and wiring.

## Data Flow

Resolved runtime settings + ordered workflow source facts/effective document +
selected repo-memory records → validated ordered provenance → prompt-task
reference/text digest binding → nested packet fields → existing activity
policy/continuation enrichment → existing packet SHA-256 →
`RuntimePromptPrepared { prompt_packet_digest, prompt_packet }` → agent prompt
using the same resolved settings and prompt-task text.

Failure before packet completion returns an error and prevents the runtime
event and agent execution.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | provenance envelope/schema insertion | `cargo test -p harness-server context_provenance_tests::runtime_packet_contains_exactly_one_versioned_provenance_manifest --lib` |
| B-002 | constructor inputs and no inventory fallback | `cargo test -p harness-server context_provenance_tests::provenance_contains_only_runtime_selected_sources --lib` |
| B-003 | ASC-001 component construction and ordered entry wrapper | `cargo test -p harness-server context_provenance_tests::all_provenance_entries_validate_against_stack_component_contract --lib` |
| B-004 | single resolved-settings construction/use | `cargo test -p harness-server context_provenance_tests::provenance_and_agent_launch_share_resolved_runtime_settings --lib` |
| B-005 | retained source list and workflow builders | `cargo test -p harness-server context_provenance_tests::central_repository_merged_and_default_workflows_have_truthful_provenance --lib` |
| B-006 | repo-memory source builder | `cargo test -p harness-server context_provenance_tests::selected_memory_order_and_safe_metadata_are_preserved --lib` |
| B-007 | empty memory input and existing degradation boundary | `cargo test -p harness-server context_provenance_tests::missing_memory_records_are_not_fabricated --lib`; `cargo test -p harness-server repo_memory_prompt --lib` |
| B-008 | dynamic-payload and prompt-task binding | `cargo test -p harness-server context_provenance_tests::prompt_task_text_is_digest_bound_without_becoming_context --lib` |
| B-009 | closed coverage markers | `cargo test -p harness-server context_provenance_tests::manifest_declares_unobserved_external_context --lib` |
| B-010 | redacted entry serialization | `cargo test -p harness-server context_provenance_tests::provenance_does_not_duplicate_memory_payload_or_secret_values --lib` |
| B-011 | ordering and digest fixtures | `cargo test -p harness-server context_provenance_tests::provenance_and_packet_digests_are_repeatable_and_order_sensitive --lib` |
| B-012 | fallible builder boundary | `cargo test -p harness-server context_provenance_tests::invalid_required_provenance_aborts_packet_construction --lib` |
| B-013 | existing RuntimePromptPrepared persistence path | `cargo test -p harness-server prompt_packet_pinning_tests --lib` |
| B-014 | existing behavior and workspace suites | `cargo test --workspace`; `git diff --name-only origin/main...HEAD` |

## Alternatives Considered

- Treat the packet digest as sufficient provenance: rejected because it cannot
  explain source identity, selection, ordering, or observation gaps.
- Build provenance from repository inventory: rejected because discovered files
  are not proof of runtime selection.
- Persist a second runtime event: rejected because packet and provenance could
  commit separately and the existing event already stores the complete packet.
- Duplicate raw memory content in provenance: rejected for privacy, bundle
  size, and inconsistent redaction risk.
- Add logic inline to `prompt_packet.rs`: rejected because the file is already
  at the repository hard ceiling.
- Claim context loaded independently by agent CLIs: rejected because Harness
  has no observation proving those loads.

## Risks

- Security: provenance could duplicate secrets or memory. Builders include
  only safe metadata and digests; negative tests inspect serialized output.
- Logic: omitting a selected source creates incomplete audit evidence. The
  constructor takes the same runtime inputs as packet construction.
- Compatibility: historical packets lack provenance. Readers must treat them
  as lower-evidence records, not invalid stored runtime events.
- Integrity: fallback-resolved settings or durable prompt text could escape the
  packet digest. One resolved value feeds both launch and provenance, and the
  packet binds prompt text before `RuntimePromptPrepared`.
- Provenance: one display-oriented workflow source string can collapse or
  misclassify central/repository inputs. The loader retains ordered typed source
  facts and exact content digests separately.
- Performance: hashing selected profile/workflow/memory representations is
  linear in packet input already being serialized and bounded by memory budget.
- Maintenance: adding a new behavior-affecting packet source must update both
  packet construction and the provenance builder in one reviewed change.

## Test Plan

- [ ] Add profile, workflow-file, workflow-default, one-memory, multi-memory,
      and no-memory provenance fixtures.
- [ ] Add resolved server-default/profile-override parity fixtures.
- [ ] Add central-only, repository-only, merged, and defaults workflow-source
      fixtures.
- [ ] Add same-reference/different-prompt-text packet digest fixtures.
- [ ] Add redaction and external-context coverage assertions.
- [ ] Add stable ordering and source/packet digest sensitivity assertions.
- [ ] Prove invalid required provenance returns before event recording and
      agent execution using the existing executor boundary test.
- [ ] Run `cargo check --workspace --all-targets`.
- [ ] Run `cargo test -p harness-server context_provenance --lib`.
- [ ] Run `cargo test -p harness-server prompt_packet --lib`.
- [ ] Run `cargo test -p harness-server repo_memory_prompt --lib`.
- [ ] With an isolated disposable PostgreSQL database, run
      `HARNESS_DATABASE_URL=<isolated-db-url> cargo test --workspace`; without
      one, run the repository pre-push DB-less suite and record the deferred DB
      suites for CI.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Before push, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.
- [ ] Confirm the implementation diff contains only the eight paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. Newly recorded prompt packets containing the
additive provenance field remain valid JSON and retain their existing packet
digest. Reversion stops adding provenance to future packets and requires no
database rollback. Evidence consumers must continue treating historical or
post-rollback packets without provenance as lower-evidence records rather than
 fabricating source claims.
