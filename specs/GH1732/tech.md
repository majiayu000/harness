# Tech Spec

## Linked Issue

GH-1732

## Product Spec

See `specs/GH1732/product.md`.

<!-- specrail-planned-changes
{"issue":1732,"complete":true,"paths":["crates/harness-server/src/http/background/runtime_profiles.rs","crates/harness-server/src/http/tests/runtime_worker_tests.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/fixtures/model_facing_prompt_v1.txt","crates/harness-server/src/workflow_runtime_worker/prompt_packet_tests.rs","crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012","B-013","B-014","B-015","B-016"]}
-->

## Current System

PR #1813 introduced durable v2 prompt packets and context provenance, then
merged with review findings and related coverage gaps unresolved:

- `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs:187-204`
  interpolates the exact profile name into a runtime locator. ASC-001 runtime
  locators accept a constrained logical-segment grammar, while
  `RuntimeProfile` accepts arbitrary names. Existing names such as
  `planning profile`, Unicode, an empty string, and UUID-shaped text can
  therefore abort packet construction. Some multi-segment names such as
  `team/codex` already validate and must keep their existing identity.
- `context_provenance_tests.rs:155-157` fixes the existing valid
  `codex-default` component ID as
  `runtime:agent_runtime:runtime_profile/codex-default`. Hashing every profile
  name would migrate that identity without a provenance-schema version change.
- `context_provenance.rs:156-164` removes three audit sections from the
  model-facing clone but leaves its schema as v2. The durable packet is
  correctly v2, but the rendered packet bytes no longer match the v1
  model-facing contract required by B-014.
- `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs:40-58`
  has only `Explicit` and `UnobservedAgentDefault` approval states, and
  `runtime_profile.rs:108-111` chooses the latter whenever the profile omits a
  policy. That claim is correct only for Codex; Claude Code and Anthropic API
  do not support this profile field.
- `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs:715-744`
  currently treats an arbitrary profile name as required-provenance failure.
  The file is already 798 lines, so additive fixtures would cross the
  repository's 800-line hard ceiling.
- `context_provenance_tests.rs:771-798` verifies that audit fields are absent
  from the rendered prompt, but does not assert the model-facing schema,
  historical removal of `workflow_file.prompt_template`, or exact
  v1-compatible prompt bytes. Deriving an expected packet from the new v2
  packet would share the implementation's field drift and is not an
  independent oracle.
- `context_provenance_tests.rs:716-768` proves only that the packet builder
  returns an error for a corrupted source digest. It does not prove that the
  executor records no `RuntimePromptPrepared` event and starts no agent.
- `crates/harness-server/src/http/background/runtime_profiles.rs:202-209` and
  `:394-407` silently convert explicitly configured approval policy to `None`
  for unsupported runtime kinds. That contradicts B-016 and hides invalid
  operator configuration before `runtime_profile.rs` can reject it.

The durable v2 packet, packet digest, activity artifact, selected-source
provenance, redaction, ordering, and runtime-setting launch parity are the
implemented B-001 through B-013 baseline. The remediation preserves that
baseline and corrects its compatibility, identity, failure-boundary, and
capability-validation gaps.

## Proposed Design

### Implemented Provenance Baseline

Keep the existing focused private
`prompt_packet/context_provenance.rs` module and the canonical ASC-001 types from
`harness_core::stack`. The module owns the v1 provenance envelope, ordered
validated components, safe locators, source digests, prompt-task binding, and
closed coverage declarations. It remains an internal prompt-packet boundary,
not a new protocol DTO.

Keep `ResolvedRuntimeSettings` as the single value shared by provenance and
agent launch. It records profile name, runtime kind, execution phase, model,
reasoning effort, sandbox, approval resolution, max turns, timeout, and stall
timeout after profile/workflow/server fallback. Claude phase-derived defaults,
explicit overrides, and lifecycle launch values continue to use this same
resolved value.

Keep the implemented source model:

- runtime settings are the first `agent_runtime` entry;
- retained central and repository workflow sources remain ordered and retain
  exact-content digests without exposing unsafe absolute paths;
- the effective workflow document, or explicit defaults when no source exists,
  uses canonical config plus prompt-template hashing;
- selected repo-memory records preserve retrieval order, safe metadata, and the
  digest of their redacted packet representation without duplicating payloads;
- prompt-task text is bound by durable reference plus exact-text SHA-256 without
  copying raw task text into provenance;
- independently loaded agent CLI, MCP, user-global, and provider context remains
  explicitly `not_observed_by_harness`.

The durable v2 packet continues to nest provenance and resolved settings before
the existing packet digest is computed. `RuntimePromptPrepared` atomically
persists the complete packet and digest, and the activity artifact carries the
same schema and digest. Any provenance validation or serialization failure
propagates before hashing, event persistence, prompt rendering, or agent
launch. Deterministic rebuild and order-sensitivity fixtures remain the
acceptance evidence for repeat/replay behavior.

### Deterministic Profile Locator

In `context_provenance.rs`, first construct the historical locator:

```text
runtime_profile/<exact profile name>
```

If `AgentStackSource::new` accepts it, use that source unchanged. This preserves
existing component IDs such as
`runtime:agent_runtime:runtime_profile/codex-default` and valid multi-segment
names such as `runtime_profile/team/codex`.

Only when the historical locator fails ASC-001 validation, derive:

```text
runtime_profile_name_sha256/<lowercase SHA-256 of exact UTF-8 profile-name bytes>
```

Use the existing `Sha256Digest::from_bytes` helper. The separate
`runtime_profile_name_sha256` namespace makes the fallback disjoint from every
historical `runtime_profile/...` locator, including a valid profile literally
named `name_sha256_<digest>`. The complete digest avoids normalization
collisions and keeps the fallback deterministic.

The locator is an audit identity, not a replacement for the setting value.
`ResolvedRuntimeSettings.profile_name` continues to serialize the exact
configured string without trimming, case folding, Unicode normalization,
escaping, or replacement. The resolved-settings digest therefore remains
sensitive to the exact name. The fallback locator does not copy the invalid raw
name; already-valid historical locators remain readable and stable.

The named fixture freezes existing component IDs and known SHA-256 outputs. It
includes an empty name, leading/trailing whitespace pairs, case-distinct invalid
names, invalid slash shapes, UUID-shaped text, and NFC/NFD Unicode pairs.
Expected digests are literal vectors produced from the exact bytes, not values
computed by the helper under test.

### Durable v2 and Model-Facing v1

Keep `RUNTIME_PROMPT_PACKET_SCHEMA` and every persisted prompt packet/activity
artifact on `harness.runtime.prompt_packet.v2`. In
`strip_model_facing_audit_sections`, after removing
`context_provenance`, `resolved_runtime_settings`, and
`prompt_task_request`, set the cloned packet's `schema` to the existing
`HISTORICAL_PROMPT_PACKET_SCHEMA_V1` constant.

`build_runtime_job_prompt` continues to remove
`workflow_file.prompt_template` before `pretty_json` and append a non-empty
template once after the packet section. Only the model-facing clone changes.
The durable packet keeps the template, provenance, resolved settings, v2 schema,
digest, event, and activity artifact.

Add
`prompt_packet/fixtures/model_facing_prompt_v1.txt`, generated from pre-v2
commit `f55eea8bb6f3355fecea2696d71e45501f973c16` with documented fixed job and
command IDs, fixed roots/profile/input, no memory, and a non-empty prompt
template. The fixture contains the complete historical prompt bytes. The named
regression test builds the current durable v2 packet from the same deterministic
inputs and compares the complete rendered prompt directly with `include_str!`.
It must not clone the v2 packet to derive the expected value and must not call
the stripping helper to construct its oracle. The test separately asserts that
the durable packet remains v2 and retains all audit fields and the template,
while the rendered packet JSON is v1, excludes the template and audit fields,
and appends the template exactly once.

### Runtime-Aware Approval Resolution

Extend private `ResolvedApprovalPolicy` in `runtime_profile.rs` with
`NotApplicable`, serialized by the existing tagged snake_case representation
as:

```json
{"resolution":"not_applicable"}
```

Resolution is a closed runtime-kind decision:

| Runtime kind | Explicit profile policy | Omitted profile policy |
| --- | --- | --- |
| `CodexExec`, `CodexJsonrpc` | `Explicit { value }` after existing validation | `UnobservedAgentDefault` |
| `ClaudeCode`, `AnthropicApi` | Typed rejection | `NotApplicable` |
| `RemoteHost` | Typed rejection before local settings resolution | Existing local-resolution rejection; no resolved settings |

`explicit_value()` returns `None` for both non-explicit variants. The two
variants remain distinct in serialized provenance: unobserved means an
effective value may be selected outside Harness, while not applicable means no
approval-policy setting participates in launch.

### Dispatch Policy Validation

In `http/background/runtime_profiles.rs`, make both top-level dispatch-policy
resolution and nested profile-override resolution return an error when the
operator explicitly supplies `approval_policy` for `ClaudeCode`,
`AnthropicApi`, or `RemoteHost`. Do not use `runtime_kind_supports_approval_policy`
to silently turn an explicit value into `None`.

An omitted policy remains capability-aware. Codex profiles may inherit a Codex
policy; non-Codex profiles carry no policy into `RuntimeProfile` and resolve to
`NotApplicable` where local settings are supported. When an override changes
from Codex to a non-Codex kind without declaring a policy, do not inherit the
Codex-only base value and do not report an error. The distinction is explicit:
unsupported operator input is rejected, while an inapplicable inherited field
is not propagated.

Table-driven tests cover explicit policy at the top-level and nested override
paths for all three non-Codex kinds, plus a kind-switch fixture proving that an
omitted non-Codex policy does not inherit a Codex value.

### Executor Failure Boundary

Keep the corrupted workflow-source digest builder test as direct evidence that a
real provenance construction error is returned. Add a separate, job-scoped
`#[cfg(test)]` failure marker in `apply_context_provenance` so a worker test can
inject the same error boundary without global state or parallel-test races. The
marker has no production build behavior and returns a named provenance error
before packet mutation or serialization.

In `http/tests/runtime_worker_tests.rs`, enqueue a job carrying that test-only
marker and run the real workflow-runtime worker with a registered recording
agent and isolated PostgreSQL database. Assert that the job fails with the
injected provenance error, no event has type `RuntimePromptPrepared`, and the
agent received zero prompts/turn invocations. This exercises the actual
executor ordering without modifying `executor.rs`.

### Test Restructuring and File Ceiling

In `context_provenance_tests.rs`, remove the obsolete invalid-profile-name
failure case, retain the corrupted workflow digest case, and replace the old
audit-field-only prompt fixture with
`profile_locator_preserves_valid_identity_and_hashes_invalid_exact_bytes`.

Put the larger frozen-prompt compatibility test in the existing
`prompt_packet_tests.rs`, backed by the read-only fixture file, rather than
embedding the golden bytes in the near-ceiling provenance test module. Put the
closed runtime-kind resolution and explicit rejection matrices in
`runtime_profile.rs`; put dispatch-policy rejection/inheritance fixtures beside
their helpers in `runtime_profiles.rs`; put the executor-boundary assertion in
the existing runtime worker integration test file.

After rustfmt, `context_provenance_tests.rs` must remain below 800 lines.
Keep `prompt_packet_tests.rs` below 800 lines as well. Delete or restructure
obsolete fixture code; do not suppress dead code, duplicate the golden bytes, or
weaken assertions.

## Authorized Implementation Surface

| Path | Change |
| --- | --- |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs` | Preserve valid historical locators, add the disjoint exact-byte hash fallback, restore v1 only on the model-facing clone, and provide the job-scoped test-only failure marker. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs` | Replace obsolete fixtures with the B-012 builder and B-015 identity/hash tests while staying below 800 lines. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet_tests.rs` | Compare the current model-facing prompt with the independent frozen pre-v2 fixture and assert durable/model separation. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/fixtures/model_facing_prompt_v1.txt` | Store the complete fixed-input prompt bytes produced by pre-v2 commit `f55eea8b`. |
| `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` | Add `NotApplicable`, resolve omitted policies by runtime kind, and close omitted/explicit runtime-kind matrices. |
| `crates/harness-server/src/http/background/runtime_profiles.rs` | Reject explicitly configured non-Codex approval policy without inheriting Codex policy across a kind switch; add focused tests. |
| `crates/harness-server/src/http/tests/runtime_worker_tests.rs` | Prove an injected provenance failure records no prompt event and starts no agent. |

No Cargo, database, protocol, workflow configuration, `executor.rs`,
`prompt_packet.rs`, or high-context file change is authorized.

## Data Flow

Exact profile name → preserve valid historical locator or use disjoint
exact-byte hash fallback + unchanged `resolved_runtime_settings.profile_name` →
durable v2 packet and digest → model-facing clone strips audit fields and
prompt template, resets schema to v1, then appends the template once → frozen
pre-v2 agent prompt contract.

Runtime kind + explicitly declared/omitted approval policy → dispatch
validation → direct runtime-setting validation → one of `Explicit`,
`UnobservedAgentDefault`, `NotApplicable`, or a typed rejection → the same
serialized resolved settings feed provenance and launch.

Injected test-only provenance failure → packet construction error → failed
runtime job with no `RuntimePromptPrepared` event → zero agent invocations.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | Existing v2 durable packet and provenance validation | `cargo test -p harness-server context_provenance_tests::v2_packet_and_artifact_share_schema_and_v1_remains_historical --lib`; isolated-DB `cargo test -p harness-server runtime_job_worker_tick_runs_registered_agent_and_completes_job --lib` |
| B-002 | Existing selected-source constructor boundary | `cargo test -p harness-server context_provenance_tests::provenance_contains_only_runtime_selected_sources --lib` |
| B-003 | ASC-001 validation plus stable valid-profile identity | `cargo test -p harness-server context_provenance_tests::all_provenance_entries_validate_against_stack_component_contract --lib`; `cargo test -p harness-server context_provenance_tests::profile_locator_preserves_valid_identity_and_hashes_invalid_exact_bytes --lib` |
| B-004 | Shared settings, Claude phase/default parity, and capability-aware approval resolution | `cargo test -p harness-server context_provenance_tests::claude_phase_defaults_and_explicit_overrides_match_agent_launch_provenance --lib`; `cargo test -p harness-server context_provenance_tests::provenance_and_agent_launch_share_resolved_runtime_settings_and_reject_zero_timeout --lib`; `cargo test -p harness-server runtime_profile::tests::approval_policy_resolution_matches_runtime_capability_matrix --lib` |
| B-005 | Existing retained workflow-source builders | `cargo test -p harness-server context_provenance_tests::central_repository_merged_and_default_workflows_have_truthful_provenance --lib` |
| B-006 | Existing repo-memory source builder | `cargo test -p harness-server context_provenance_tests::selected_memory_order_and_safe_metadata_are_preserved --lib` |
| B-007 | Missing-memory and retrieval-degradation boundary | `cargo test -p harness-server context_provenance_tests::missing_memory_records_are_not_fabricated --lib`; `cargo test -p harness-server repo_memory_prompt::tests::memory_flag_degraded_enabled_store_unavailable_records_degradation --lib` |
| B-008 | Existing prompt-task digest binding | `cargo test -p harness-server context_provenance_tests::prompt_task_text_is_digest_bound_without_becoming_context --lib` |
| B-009 | Existing closed coverage markers | `cargo test -p harness-server context_provenance_tests::manifest_declares_unobserved_external_context --lib` |
| B-010 | Existing redacted serialization | `cargo test -p harness-server context_provenance_tests::provenance_does_not_duplicate_memory_payload_or_secret_values --lib` |
| B-011 | Deterministic rebuild/replay, order sensitivity, and exact-byte locator vectors | `cargo test -p harness-server context_provenance_tests::provenance_and_packet_digests_are_repeatable_and_order_sensitive --lib`; `cargo test -p harness-server context_provenance_tests::profile_locator_preserves_valid_identity_and_hashes_invalid_exact_bytes --lib` |
| B-012 | Real builder failure plus executor fail-closed ordering | `cargo test -p harness-server context_provenance_tests::invalid_required_provenance_aborts_packet_construction --lib`; isolated-DB `cargo test -p harness-server provenance_failure_prevents_prompt_event_and_agent_start --lib` |
| B-013 | Runtime worker event/artifact linkage | isolated-DB `cargo test -p harness-server runtime_job_worker_tick_runs_registered_agent_and_completes_job --lib` |
| B-014 | Independent frozen v1 complete-prompt compatibility | `cargo test -p harness-server prompt_packet::tests::model_facing_prompt_matches_frozen_v1_fixture_while_durable_packet_remains_v2 --lib` |
| B-015 | Stable valid identity plus disjoint exact-byte fallback | `cargo test -p harness-server context_provenance_tests::profile_locator_preserves_valid_identity_and_hashes_invalid_exact_bytes --lib` |
| B-016 | Closed runtime-kind resolution and dispatch rejection matrices | `cargo test -p harness-server runtime_profile::tests::approval_policy_resolution_matches_runtime_capability_matrix --lib`; `cargo test -p harness-server runtime_profile::tests::runtime_profile_approval_policy_rejects_non_codex_runtimes --lib`; `cargo test -p harness-server runtime_profiles::tests::runtime_dispatch_rejects_explicit_non_codex_approval_policy --lib`; `cargo test -p harness-server runtime_profiles::tests::runtime_dispatch_kind_switch_does_not_inherit_codex_approval_policy --lib` |

## Alternatives Considered

- Percent-encode or slugify profile names: rejected because ambiguous
  normalization and reserved segments can still create collisions or invalid
  locators.
- Hash every profile name under the historical namespace: rejected because it
  migrates valid component IDs and can collide with a valid raw profile name
  shaped like the fallback.
- Reject or rename existing profiles: rejected because provenance must not
  narrow the already accepted runtime-profile contract.
- Downgrade the durable packet to v1: rejected because v2 is the required
  evidence contract; only the model-facing compatibility clone is v1.
- Derive the v1 expected packet by deleting fields from the new v2 packet:
  rejected because actual and oracle would share unrelated field drift.
- Reuse `UnobservedAgentDefault` for every omitted policy: rejected because it
  falsely claims an external default exists for unsupported runtimes.
- Silently drop explicit non-Codex approval policy in dispatch configuration:
  rejected because invalid operator input must not become success-shaped.
- Add a global failure flag for the executor test: rejected because parallel
  tests could observe another test's injected state; the job-scoped marker is
  deterministic and isolated.

## Risks

- Identity: unconditional hashing would split a stable component identity.
  Valid historical locators are preserved; only previously invalid names enter
  the disjoint namespace.
- Privacy: hashing is not encryption. This change does not claim secrecy; it
  avoids copying an invalid arbitrary name into the fallback locator while the
  exact name remains in resolved settings.
- Compatibility: changing only the model-facing clone must not mutate or
  re-hash the durable packet. The frozen complete-prompt fixture and durable
  assertions cover both sides without a shared oracle.
- Semantics: `NotApplicable` and `UnobservedAgentDefault` could be conflated by
  callers that only use `explicit_value()`. Serialized enum assertions preserve
  their distinct audit meaning.
- Configuration: changing silent discard to a typed error can reject previously
  accepted invalid configurations. This is intentional fail-closed behavior and
  the error must name the runtime kind and unsupported field.
- Test integrity: the failure seam exists only under `cfg(test)` and is scoped
  to one job input; production builds expose no trigger and parallel tests share
  no mutable flag.
- Maintenance: the provenance test file is at the ceiling. Replacement and a
  separate read-only golden fixture, not additive inline snapshots, are
  completion requirements.

## Test Plan

- [ ] Add
      `profile_locator_preserves_valid_identity_and_hashes_invalid_exact_bytes`;
      freeze existing IDs for `codex-default` and `team/codex`, then assert
      literal SHA-256 vectors for empty, whitespace-, case-, slash-, UUID-, and
      NFC/NFD-distinct invalid names plus exact preserved profile values.
- [ ] Generate `model_facing_prompt_v1.txt` from pre-v2 commit `f55eea8b` using
      fixed IDs/inputs and a non-empty prompt template; document the inputs in
      the test and compare current complete prompt bytes directly to the
      fixture.
- [ ] Add
      `model_facing_prompt_matches_frozen_v1_fixture_while_durable_packet_remains_v2`;
      assert durable v2 and audit/template retention, rendered v1 and field
      stripping, exact complete prompt bytes, and one appended template section.
- [ ] Add
      `approval_policy_resolution_matches_runtime_capability_matrix` for both
      Codex kinds, Claude Code, Anthropic API, and Remote Host; expand explicit
      direct-resolution rejection over every non-Codex kind.
- [ ] Add top-level and nested dispatch-policy matrices that reject explicit
      policy for Claude Code, Anthropic API, and Remote Host; add a Codex to
      non-Codex kind-switch case that does not inherit Codex policy.
- [ ] Retain B-012 coverage for corrupted required provenance after removing
      invalid profile names as a negative case.
- [ ] Add the job-scoped `cfg(test)` failure marker and isolated-DB
      `provenance_failure_prevents_prompt_event_and_agent_start` worker test.
- [ ] Re-run the existing Claude phase/default and shared launch-settings
      parity tests without weakening assertions.
- [ ] Re-run the missing-memory and visible degradation-artifact tests.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo test -p harness-server context_provenance --lib`.
- [ ] Run `cargo test -p harness-server prompt_packet --lib`.
- [ ] Run `cargo test -p harness-server runtime_profile --lib`.
- [ ] Run `cargo test -p harness-server runtime_profiles --lib`.
- [ ] Run `cargo test -p harness-server repo_memory_prompt --lib`.
- [ ] Run `cargo test -p harness-server --lib`.
- [ ] With an isolated disposable PostgreSQL database, run
      `HARNESS_DATABASE_URL=<isolated-test-url> scripts/test-server-db.sh` and
      `HARNESS_DATABASE_URL=<isolated-test-url> cargo test --workspace -- --test-threads=1`.
      Without one, run the repository DB-less pre-push suite, record the
      PostgreSQL suites as deferred, and require current-head CI or an isolated
      database run before merge readiness.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` before push.
- [ ] Verify both `context_provenance_tests.rs` and `prompt_packet_tests.rs` are
      below 800 lines.
- [ ] Verify the implementation diff contains exactly the seven authorized
      implementation paths.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.

## Rollback Plan

Revert the remediation implementation commit and the frozen fixture together.
No migration or external dependency rollback is required. The rollback would
restore invalid-name failures, identity/prompt incompatibility, silent
non-Codex policy discard, and the unproven executor boundary, so it is
acceptable only as an emergency response followed by blocking affected runtime
submissions. Existing durable v2 packets remain valid JSON and retain their
recorded digests.
