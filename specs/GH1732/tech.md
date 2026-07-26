# Tech Spec

## Linked Issue

GH-1732

## Product Spec

See `specs/GH1732/product.md`.

<!-- specrail-planned-changes
{"issue":1732,"complete":true,"paths":["crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs","crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs","crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs"],"spec_refs":["B-004","B-012","B-014","B-015","B-016"]}
-->

## Current System

PR #1813 introduced durable v2 prompt packets and context provenance, then
merged with three review findings unresolved:

- `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs:187-204`
  interpolates the exact profile name into a runtime locator. ASC-001 runtime
  locators accept a constrained logical-segment grammar, while
  `RuntimeProfile` accepts arbitrary names. Existing names such as
  `planning profile`, `team/codex`, Unicode, and UUIDs can therefore abort
  packet construction.
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
  from the rendered prompt, but does not assert the model-facing schema or
  exact v1-compatible packet bytes.

The durable v2 packet, packet digest, activity artifact, and all other
provenance sources are correct and remain unchanged by this remediation.

## Proposed Design

### Deterministic Profile Locator

In `context_provenance.rs`, derive the runtime-profile source locator as:

```text
runtime_profile/name_sha256_<lowercase SHA-256 of exact UTF-8 profile-name bytes>
```

Use the existing `Sha256Digest::from_bytes` helper. The fixed
`name_sha256_` prefix makes the second locator segment valid even when the
digest happens to resemble another reserved identifier. The complete digest
avoids normalization collisions and keeps the mapping deterministic.

The locator is an audit identity, not a replacement for the setting value.
`ResolvedRuntimeSettings.profile_name` continues to serialize the exact
configured string without trimming, case folding, escaping, or replacement.
The resolved-settings digest therefore remains sensitive to the exact name.
No raw profile name is copied into the locator.

### Durable v2 and Model-Facing v1

Keep `RUNTIME_PROMPT_PACKET_SCHEMA` and every persisted prompt packet/activity
artifact on `harness.runtime.prompt_packet.v2`. In
`strip_model_facing_audit_sections`, after removing
`context_provenance`, `resolved_runtime_settings`, and
`prompt_task_request`, set the cloned packet's `schema` to the existing
`HISTORICAL_PROMPT_PACKET_SCHEMA_V1` constant.

Only the clone passed to `pretty_json` changes. The durable packet, provenance
validation, digest, event, and activity artifact remain v2. A named regression
test builds the v2 durable packet, constructs the expected pre-v2 model packet
by removing the audit-only fields and assigning v1, and proves that the
rendered prompt contains exactly that v1 JSON while the original packet stays
v2.

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
| `ClaudeCode`, `AnthropicApi` | Existing typed rejection | `NotApplicable` |
| `RemoteHost` | Existing server-side rejection; no resolved settings | Existing server-side rejection |

`explicit_value()` returns `None` for both non-explicit variants. The two
variants remain distinct in serialized provenance: unobserved means an
effective value may be selected outside Harness, while not applicable means no
approval-policy setting participates in launch.

### Test Restructuring and File Ceiling

Keep all provenance fixtures in the existing
`context_provenance_tests.rs`, but replace the obsolete invalid-profile-name
half of `invalid_required_provenance_aborts_packet_construction` with
`arbitrary_profile_names_use_stable_hashed_locators_and_preserve_profile_name`.
Retain the corrupted workflow digest half as the B-012 failure fixture, moving
or shortening helpers when necessary.

Replace the current audit-field-only prompt fixture with
`model_facing_prompt_uses_v1_schema_while_durable_packet_remains_v2`; the new
test subsumes the old assertions. Put the small runtime-kind approval matrix
test in `runtime_profile.rs` as
`non_codex_omitted_approval_policy_is_not_applicable`.

After rustfmt, `context_provenance_tests.rs` must remain below 800 lines.
Delete or restructure obsolete fixture code; do not suppress dead code or
weaken assertions.

## Authorized Implementation Surface

| Path | Change |
| --- | --- |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs` | Hash arbitrary profile names for locators and restore v1 schema only in the model-facing clone. |
| `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs` | Replace obsolete fixtures with the named B-014 and B-015 acceptance tests while staying below 800 lines. |
| `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` | Add `NotApplicable`, resolve omitted policies by runtime kind, and add the named B-016 test. |

No Cargo, database, protocol, workflow configuration, executor, prompt-packet
call-site, or high-context file change is authorized.

## Data Flow

Exact profile name → SHA-256 locator identity + unchanged
`resolved_runtime_settings.profile_name` → durable v2 packet and digest →
model-facing clone strips audit fields and resets schema to v1 → unchanged
agent prompt contract.

Runtime kind + declared approval policy → existing validation → one of
`Explicit`, `UnobservedAgentDefault`, or `NotApplicable` → the same serialized
resolved settings feed provenance and launch.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | Existing v2 durable packet and provenance validation | `cargo test -p harness-server context_provenance_tests::v2_packet_and_artifact_share_schema_and_v1_remains_historical --lib` |
| B-002 | Existing selected-source constructor boundary | `cargo test -p harness-server context_provenance_tests::provenance_contains_only_runtime_selected_sources --lib` |
| B-003 | Existing ASC-001 component validation | `cargo test -p harness-server context_provenance_tests::all_provenance_entries_validate_against_stack_component_contract --lib` |
| B-004 | Runtime-aware approval resolution preserves shared resolved settings | `cargo test -p harness-server runtime_profile::tests::non_codex_omitted_approval_policy_is_not_applicable --lib`; `cargo test -p harness-server context_provenance_tests::codex_omitted_approval_policy_is_recorded_unobserved --lib` |
| B-005 | Existing retained workflow-source builders | `cargo test -p harness-server context_provenance_tests::central_repository_merged_and_default_workflows_have_truthful_provenance --lib` |
| B-006 | Existing repo-memory source builder | `cargo test -p harness-server context_provenance_tests::selected_memory_order_and_safe_metadata_are_preserved --lib` |
| B-007 | Existing missing-memory degradation boundary | `cargo test -p harness-server context_provenance_tests::missing_memory_records_are_not_fabricated --lib` |
| B-008 | Existing prompt-task digest binding | `cargo test -p harness-server context_provenance_tests::prompt_task_text_is_digest_bound_without_becoming_context --lib` |
| B-009 | Existing closed coverage markers | `cargo test -p harness-server context_provenance_tests::manifest_declares_unobserved_external_context --lib` |
| B-010 | Existing redacted serialization | `cargo test -p harness-server context_provenance_tests::provenance_does_not_duplicate_memory_payload_or_secret_values --lib` |
| B-011 | Existing deterministic digest fixtures plus B-015 locator fixture | `cargo test -p harness-server context_provenance_tests::provenance_and_packet_digests_are_repeatable_and_order_sensitive --lib`; `cargo test -p harness-server context_provenance_tests::arbitrary_profile_names_use_stable_hashed_locators_and_preserve_profile_name --lib` |
| B-012 | Retained corrupted-source failure fixture | `cargo test -p harness-server context_provenance_tests::invalid_required_provenance_aborts_packet_construction --lib` |
| B-013 | Existing runtime-worker persistence integration | `cargo test -p harness-server runtime_job_worker_tick_runs_registered_agent_and_completes_job --lib` |
| B-014 | Model-facing schema and byte compatibility | `cargo test -p harness-server context_provenance_tests::model_facing_prompt_uses_v1_schema_while_durable_packet_remains_v2 --lib` |
| B-015 | Hashed locator and exact-name preservation | `cargo test -p harness-server context_provenance_tests::arbitrary_profile_names_use_stable_hashed_locators_and_preserve_profile_name --lib` |
| B-016 | Capability-aware omitted approval policy | `cargo test -p harness-server runtime_profile::tests::non_codex_omitted_approval_policy_is_not_applicable --lib`; `cargo test -p harness-server runtime_profile::tests::runtime_profile_approval_policy_rejects_non_codex_runtimes --lib` |

## Alternatives Considered

- Percent-encode or slugify profile names: rejected because ambiguous
  normalization and reserved segments can still create collisions or invalid
  locators.
- Reject or rename existing profiles: rejected because provenance must not
  narrow the already accepted runtime-profile contract.
- Downgrade the durable packet to v1: rejected because v2 is the required
  evidence contract; only the model-facing compatibility clone is v1.
- Reuse `UnobservedAgentDefault` for every omitted policy: rejected because it
  falsely claims an external default exists for unsupported runtimes.
- Add another test file: rejected because the authorized surface is fixed and
  the obsolete fixtures can be replaced without exceeding the file ceiling.

## Risks

- Identity: hashing hides readable names in locators. The exact name remains in
  resolved settings, while the full SHA-256 locator is stable and
  collision-resistant.
- Privacy: hashing is not encryption. This change does not claim secrecy; it
  prevents raw arbitrary text from becoming a locator.
- Compatibility: changing only the model-facing clone must not mutate or
  re-hash the durable packet. The named B-014 test asserts both objects.
- Semantics: `NotApplicable` and `UnobservedAgentDefault` could be conflated by
  callers that only use `explicit_value()`. Serialized enum assertions preserve
  their distinct audit meaning.
- Maintenance: the test file is at the ceiling. Replacement, not additive
  fixtures, is a completion requirement.

## Test Plan

- [ ] Add
      `arbitrary_profile_names_use_stable_hashed_locators_and_preserve_profile_name`
      with spaces, slash, Unicode, and UUID-shaped names; assert valid stable
      locators, distinct test outputs, and exact preserved profile names.
- [ ] Replace the old model-facing fixture with
      `model_facing_prompt_uses_v1_schema_while_durable_packet_remains_v2`;
      assert durable v2, rendered v1, removed audit fields, and v1-compatible
      packet bytes.
- [ ] Add
      `non_codex_omitted_approval_policy_is_not_applicable` for Claude Code and
      Anthropic API; retain Codex-unobserved and explicit-non-Codex rejection
      tests.
- [ ] Retain B-012 coverage for corrupted required provenance after removing
      invalid profile names as a negative case.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo test -p harness-server context_provenance --lib`.
- [ ] Run `cargo test -p harness-server runtime_profile --lib`.
- [ ] Run `cargo test -p harness-server --lib`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` before push.
- [ ] Verify `context_provenance_tests.rs` is below 800 lines.
- [ ] Verify the implementation diff contains exactly the three authorized
      implementation paths.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.

## Rollback Plan

Revert the remediation implementation commit. No migration or external
dependency rollback is required. The rollback would restore the three known
defects, so it is acceptable only as an emergency response followed by
blocking affected runtime submissions. Existing durable v2 packets remain
valid JSON and retain their recorded digests.
