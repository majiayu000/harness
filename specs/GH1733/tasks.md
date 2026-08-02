# Task Plan

## Linked Issue

GH-1733

## Spec Packet

- Product: `specs/GH1733/product.md`
- Runtime product: `specs/GH1733/runtime-product.md`
- Runtime observation: `specs/GH1733/runtime-observation.md`
- Runtime supervision: `specs/GH1733/runtime-supervision.md`
- Tech: `specs/GH1733/tech.md`
- Tasks and product-to-test map: `specs/GH1733/tasks.md`

## Readiness Gate

This plan is not implementation approval. GH-1733 is currently
`ready_to_spec`. Do not modify PR #1859 production code until maintainers
approve all six files in this packet and record `ready_to_implement`. Once
approved, amend the original PR branch only; do not create a replacement
implementation PR or force-push.

## Implementation Tasks

- [ ] `SP1733-T1` — Owner: core fingerprint model worker. Dependencies: approved product and tech specs plus `ready_to_implement`. Covers: B-001, B-003, B-008, B-011 through B-015. Done when: the strict outer envelope carries a canonical `fingerprint_digest` separate from ASC-001 exact-source-byte component integrity; its exact domain, three `u64` frames, string escaping, raw-number preservation, two framing vectors, and one full valid vector per subject are frozen independently; closed runtime/MCP payloads, probe and lifecycle-cleanup failures, injective runtime-role and configured-server-scoped MCP tool-source derivation, exact bounded MCP text, optional presence-sensitive raw-object `annotations`, required `inputSchema`, optional presence-sensitive `outputSchema`, and raw-JSON-only duplicate-aware object-root schema parsing are implemented in the split core modules. The core manifest explicitly enables existing `serde_json/raw_value`; a borrowed `RawValue` recursive visitor preserves exact validated number lexemes without a handwritten lexer, new package, or lockfile change. The schema parser defaults absent `$schema` to Draft 2020-12, accepts only the two exact Draft 2020-12 and Draft-07 identifiers, rejects unknown/non-string/nested dialect declarations, and applies `contentSchema`, modern dependency/prefix keywords, legacy `dependencies`, tuple `items`, and `additionalItems` only under the selected dialect; Draft-07 `contentSchema` remains ordered instance data. Core owns typed `RuntimeRoleSourceBinding::derive` and strict `parse`; exact source bytes and raw envelopes are each bounded at 2,097,152 before copy/hash or JSON allocation, base source locators at 4,096 UTF-8 bytes, and complete derived locators at 8,259; the four closed limit reasons and precedence are frozen, and configured MCP server identity uses the exact bounded stable key with the frozen HT/LF/CR/SP blank predicate. Every non-object schema root and every invalid subject/payload/source, dialect, capability, ordering, integrity, fingerprint-digest, or fixed resource-limit combination fails typed; callers cannot supply generic serializable/schema maps; constructors and parsers require an empty capability list; exact and limit-plus-one vectors cover source/locator/envelope sizes, stable-key blank bytes, number spellings, root depth, value nodes, decoded strings, direct entries, raw bytes, and canonical bytes; every core file is below 800 lines. Verify: `cargo test -p harness-core fingerprint`, `cargo test -p harness-core stack`, `cargo test -p harness-core`, `cargo check -p harness-core --all-targets`, and `cargo tree -e features -p harness-core`.
      The schema transition table additionally treats `not`, `if`, `then`,
      `else`, `contains`, `propertyNames`, and `additionalProperties` as
      object/boolean schema positions in both dialects, and Draft-07
      `additionalItems` as schema-valued even without tuple `items`; malformed
      shapes and nested `$schema` fail with closed details.
      Schema v0.1 constructors and parsers reject every Windows command form or
      present Windows resolution context; Windows resolver/digest helpers remain
      pure contract values and cannot construct unreachable envelope evidence.
- [ ] `SP1733-T2` — Owner: runtime fingerprint worker. Dependencies:
      SP1733-T1. Covers: B-002 through B-010, B-014, and B-015. Done when:
      isolation and exact passthrough `DangerFullAccess` gates precede all host
      observation; supported Linux proves descriptor isolation, pidfd and
      ptrace capability, while other platforms fail before cwd access.
      The owner is the sole target/helper creator, ptrace controller, waiter,
      and reaper except for the audited target `PTRACE_TRACEME`. Every child is
      gated until its exact pidfd plus reap obligation is atomically registered.
      Before registry commit, rollback may use only the still-unreaped direct
      child's positive PID; afterward every signal uses the registered pidfd,
      and after capability wait/reap succeeds every later wait/reap is
      pidfd-only. The sole exception is exact-PID reap of the failed initial
      capability bootstrap while its pidfd remains held. There is no anchor,
      PGID, process-group membership, negative-PID,
      post-reap-PID, or descendant-enumeration evidence.
      Eight owners each reserve two pidfd and 28 non-pidfd slots. Before
      `DescriptorsReady`, at most one bootstrap child per owner may
      transiently inherit the process-wide fd table in addition to an admitted
      target; it performs no workload and is not numerically ledger-bounded.
      The active deadline bounds readiness waiting; rollback uses the cleanup
      deadline, with obligation and permit retained until reap. After readiness one child retains at most 12
      allowlisted references; a post-exec target plus observer retains at most
      eight. No other child-role concurrency is legal, proving post-ready
      ceilings of 40 per fingerprint, 16 pidfds and 320 descriptors globally.
      Self-pidfd open/signal plus the capability child's validating and
      consuming `waitid(P_PIDFD)` calls must pass before cwd. The active deadline covers
      retained cwd/target observation, authorization, exec stop, target
      execution/reap, bounded output, and post-reap checkpoint; cleanup has its
      separate deadline. Success requires exact target reap, complete bounded
      streams, a passing post-reap checkpoint, and an empty registry.
      The post-exec guard denies process creation, image execution, executable
      mapping (including x86_64 `uselib`), image mutation, and signalling
      before execution. The claim is registry-empty plus no executed
      process-creation syscall, never descendant-tree-empty.
      Cwd is retained with `O_PATH | O_DIRECTORY | O_CLOEXEC` and no
      `O_NOFOLLOW`; search-only cwd and pathname replacement are tested.
      Initial and retry supervision setup stages are exactly
      `working_directory_enter` and `trace_setup`. Capture failures have
      precedence; after complete capture, signal/nonzero is the sole semantic
      failure, and only zero exit reaches UTF-8/blank/grammar selection.
      Existing bounded PATH, `ETXTBSY`, fd-10 `execveat`, ELF, environment,
      repository authorization, fail-closed, and no-shell contracts remain.
      Verify: `cargo test -p harness-agents runtime_fingerprint`,
      `cargo test -p harness-agents`, and
      `cargo check -p harness-agents --all-targets`.
- [ ] `SP1733-T3` — Owner: boundary contract worker. Dependencies: SP1733-T1 and SP1733-T2. Covers: B-002 and B-016. Done when: a `#[cfg(test)]`-only exhaustive workflow `RuntimeKind` mapping proves the three local kinds map one-to-one, `AnthropicApi`/`RemoteHost` are not local executables, and no non-host isolation can be interpreted as a host fingerprint subject; production call-site audit proves there is no snapshot, server, workflow-runtime, task-runner, `CodeAgent`, `AgentAdapter`, CLI, HTTP, persistence, or migration consumer; and the implementation diff matches the fourteen authorized paths exactly with no lockfile change. Verify: `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib` plus the manifest and `rg` audits described below.
- [ ] `SP1733-T4` — Owner: verification and handoff owner.
      Dependencies: SP1733-T1 through SP1733-T3. Covers: B-001 through B-016.
      Done when formatting, focused/package/workspace tests, clippy,
      file-size/manifest/API/call-site audits, current-head independent review,
      Gemini, and ruleset approval pass; every #1862 thread and valid #1859
      finding is re-evaluated on the exact head. Verification must prove
      owner-exclusive process creation/ptrace/wait/reap; atomic pidfd
      registration before `GO`; direct-child rollback before registration;
      two-pidfd/28-non-pidfd owner ledgers; validating and consuming
      `waitid(P_PIDFD)` on the capability child, bootstrap fallback, then
      exact-pidfd-only cleanup; registry
      emptiness on success; and absence of anchors, PGID signalling,
      membership stages, or descendant-tree claims. It must also cover
      an unrelated same-session process repeatedly changing process groups
      without entering the registry, being observed/signalled, or changing
      success evidence;
      `O_PATH` search-only cwd, both setup stages on initial/retry targets,
      x86_64 `uselib` denial without an aarch64 pseudo-entry, output failure
      precedence, fatal/caught/ignored signal delivery and illegal trace
      transitions, static ELF success and all closed format rejections,
      exec-stop/hash verification, repository non-execution, platform gates,
      exact digest vectors, `ETXTBSY`, schema dialect behavior, source
      binding, annotation bounds, and unchanged producer-only scope.
      The original PR may close GH-1733 only after all gates pass. Verify every
      command and audit in Required Verification on one current implementation
      head, then collect fresh PR-gate evidence.

## Ownership and Ordering

Tasks run in dependency order. A later task may begin only after the dependency
task has committed a passing head. If multiple agents are used, their writable
files remain disjoint exactly as follows.

| Task | Writable files |
| --- | --- |
| SP1733-T1 | `crates/harness-core/Cargo.toml`; `crates/harness-core/src/stack/mod.rs`; `crates/harness-core/src/stack/fingerprint.rs`; `crates/harness-core/src/stack/fingerprint/model.rs`; `crates/harness-core/src/stack/fingerprint/schema.rs`; `crates/harness-core/src/stack/fingerprint/tests.rs` |
| SP1733-T2 | `crates/harness-agents/Cargo.toml`; `crates/harness-agents/src/lib.rs`; `crates/harness-agents/src/runtime_fingerprint.rs`; `crates/harness-agents/src/runtime_fingerprint/environment.rs`; `crates/harness-agents/src/runtime_fingerprint/executable.rs`; `crates/harness-agents/src/runtime_fingerprint/probe.rs`; `crates/harness-agents/src/runtime_fingerprint/tests.rs` |
| SP1733-T3 | `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` (`#[cfg(test)]` contract only) |
| SP1733-T4 | Read-only verification, review-thread resolution, and original-branch handoff; no writable source files |

No other implementation path is authorized. The agents manifest may add only
the existing workspace `libc`; the core manifest may only enable
`serde_json/raw_value` on its existing workspace dependency. Do not edit any
other Cargo file, the lockfile,
database or persistence code, configuration schemas, adapter launch paths,
snapshots, prompts, CLI/HTTP surfaces, or high-context files. A newly
discovered need outside this manifest returns the change to spec review instead
of silently expanding scope.

## Required Verification

- [ ] Run `cargo fmt --all`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo check -p harness-core -p harness-agents --all-targets`.
- [ ] Run `cargo test -p harness-core fingerprint`.
- [ ] Run `cargo test -p harness-core stack`.
- [ ] Run `cargo test -p harness-core`.
- [ ] Run `cargo test -p harness-agents runtime_fingerprint`.
- [ ] Run `cargo test -p harness-agents`.
- [ ] Run
      `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib`.
- [ ] Run `cargo check --workspace --all-targets`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo audit`.
- [ ] Run `git diff --check`.
- [ ] Confirm every changed Rust file is below 800 lines after rustfmt.
- [ ] Confirm the implementation changed-file set equals the fourteen paths in
      the tech-spec `specrail-planned-changes` manifest.
- [ ] Confirm `Cargo.lock` is unchanged and `cargo tree -p harness-agents -i
      libc` resolves the existing pinned workspace dependency; confirm
      `cargo tree -e features -p harness-core` shows direct
      `serde_json/raw_value`.
- [ ] Use `rg` to prove the new producer APIs have no production consumer
      outside their defining modules; `#[cfg(test)]` references do not count.
- [ ] Use `rg` to prove the public MCP schema evidence API exposes no
      `from_serializable`, `serde_json::Value`, or typed-map constructor.
- [ ] Run the repository SpecRail structural checker if it exists on the
      implementation base. If it remains removed from current `main`, perform
      an independent structural review of the complete manifest and B-001
      through B-016 coverage and do not claim that a removed checker passed.
- [ ] Re-evaluate every current review thread on PR #1862 and PR #1859;
      resolve only findings demonstrably addressed on the current head and post
      no comment or reply without separate authorization.
- [ ] Wait for current-head CI, independent native review, Gemini review, and
      repository ruleset approval before merge.
- [ ] Obtain mandatory human security review of the effective-sandbox
      fail-closed gate; Linux observation-process fixed-frame/`SCM_RIGHTS`
      protocol; bounded launch/environment/setup-secret counting; eight-owner
      admission and permit lifetime; owner/helper/child fd ledgers; allocation-
      free post-fork work; bounded pre-ready inherited-fd transient and
      foreign-fd isolation at/after `DESCRIPTORS_READY`;
      pre-fork start gates and direct-child rollback; pidfd registration,
      `waitid(P_PIDFD, WNOWAIT)` validation, consuming pidfd reap, bootstrap
      exact-PID fallback, revalidation, handoff, and reap ownership; retained cwd/target descriptors
      and `fchdir`; final-target authorization; `FD_CLOEXEC` shebang rejection;
      ptrace exec-stop/first-instruction ordering and hash/image validation under
      kernel write denial; W+X/executable-stack rejection; post-exec
      syscall-stop denial of process/image/executable-mapping (including
      x86_64 `uselib`) and existing-executable-mutation transitions,
      target-initiated process signalling, and pre-native x32 rejection;
      legal signal-delivery reinjection and illegal-state rejection;
      retained-handle pre-exec; exact registered-pidfd-only signalling and reap
      ordering; empty-registry success;
      argument/environment pointer ownership; NUL validation; stage-tagged
      errno propagation; absence of PGID/membership evidence; and proof that `ENOEXEC`
      cannot invoke a shell.

## Product-to-Test Mapping

| Product behavior | Required verification |
| --- | --- |
| B-001, B-014, B-015 | `envelope_round_trips_both_closed_subjects`; `envelope_rejects_version_subject_payload_capability_and_fingerprint_digest_mismatch`; `fingerprint_digest_is_separate_from_component_integrity`; `fingerprint_digest_framing_vectors_are_independent`; `complete_runtime_and_mcp_payload_digest_vectors_are_fixed`; `canonical_payload_string_escaping_is_frozen`; `canonical_payload_preserves_raw_json_number_tokens`; `failure_payload_changes_fingerprint_digest_without_fabricating_integrity`; `component_integrity_preserves_exact_source_bytes_or_absence` |
| B-002 | `local_executable_runtime_kind_is_closed_and_uses_fixed_args_and_output_grammars`; `container_isolation_fails_before_host_resolution`; `microvm_isolation_fails_before_host_resolution`; `sandbox_passthrough_state_is_only_supported_policy`; `restricted_sandbox_fails_before_host_observation`; `narrowed_allowed_write_paths_fail_before_host_observation`; server `runtime_fingerprint_runtime_kind_contract_is_exhaustive` |
| B-003, B-011 | `runner_observation_preserves_every_runtime_and_mcp_source_identity`; `runtime_role_sources_are_pairwise_distinct_for_one_base`; `runtime_role_source_preserves_scope_and_exact_source_integrity_or_absence`; `caller_cannot_preencode_or_override_runtime_role_source`; `runtime_role_parser_rejects_missing_malformed_noncanonical_and_wrong_role_suffixes`; `repository_owned_runtime_never_spawns_version_child`; `caller_cannot_promote_repository_source`; `configured_mcp_server_binding_uses_exact_stable_key`; `configured_mcp_server_key_accepts_1024_and_rejects_1025_before_expansion`; `arbitrary_mcp_server_component_is_not_accepted`; `distinct_mcp_server_keys_have_distinct_ids`; `mcp_tool_source_is_injective_for_multiple_tools_on_one_server`; `mcp_tool_source_preserves_scope_and_encodes_exact_utf8_identity`; `mcp_server_and_tool_suffix_mismatches_are_rejected`; `caller_cannot_supply_preencoded_mcp_tool_source` |
| B-004 | Frozen Unix/Windows command-form and digest vectors; `O_PATH` retained cwd with no `O_NOFOLLOW`; search-only cwd and pathname replacement; raw Unix bytes and Windows UTF-16 units; exact `EACCES` fallback, one 150 ms `ETXTBSY` retry, candidate 65, no shell, and fd-10 execution context; later pre-target outcomes preserve prior reaped attempts while the registry is empty |
| B-005, B-010 | Closed environment policy, setup-secret exclusion, exact PATH and Claude-directory digest vectors, Unix/Windows key rules, direct/env shebang rejection before target/interpreter creation, and proof that only sanitized PATH reaches an admitted static target |
| B-006 | Nonblocking retained executable handle, size and strong-identity checkpoints, hard-link authorization, kill-isolated observation helpers, atomic pidfd registration before `GO`, exact observation errors, static ELF/exec-stop verification, path-race rejection, and no in-process blocking worker |
| B-007 | Eight-owner deadlines; bounded pre-ready inherited-fd transient plus post-ready 28 + 12 retained capacity; two pidfds per owner; capability-child validating/consuming `waitid(P_PIDFD)`, bootstrap exact-PID fallback, then exact-pidfd-only signal/reap; initial/retry `working_directory_enter` and `trace_setup`; process/image/mapping/mutation/signalling denial including x86_64 `uselib`; target reap + complete output + post-reap checkpoint + empty registry success barrier; unrelated same-session `setpgid` churn is never registered/observed/signalled and cannot affect success; no anchor/PGID/membership/descendant-tree claim; non-Linux pre-observation failure |
| B-008 | Closed failure vocabulary and canonical ordering; observation errors remain no-envelope; termination/reap/drain cleanup failures retain exact ownership; removed `lingering_process_group`, anchor, and membership values are rejected |
| B-009 | Exact Codex/Claude whole-stream grammars and stream selection; capture error precedence; after complete capture signal/nonzero exclusivity; zero-exit-only UTF-8/blank/grammar classification; exact HT/LF/CR/SP blank predicate and success-only output digests |
| B-012 | `mcp_description_preserves_absent_empty_space_tab_and_newline_distinctions`; `mcp_output_schema_absence_and_presence_are_distinct`; `mcp_annotations_preserve_absent_empty_hints_title_vendor_values_and_ordered_arrays`; `mcp_annotation_hints_do_not_infer_capabilities`; exact-limit and limit-plus-one tool-name/description/annotations fixtures |
| B-013 | `mcp_input_schema_rejects_every_non_object_root`; `mcp_output_schema_rejects_malformed_and_every_non_object_root`; `mcp_output_schema_applies_every_exact_and_limit_plus_one_bound`; `absent_schema_dialect_defaults_to_draft_2020_12`; `exact_supported_schema_dialects_round_trip`; `unknown_nonstring_and_nested_schema_dialects_fail_typed`; `schema_set_locations_reorder_canonically`; Draft 2020-12 `content_schema_traverses_nested_required_and_one_of_as_schema`; Draft-07 `content_schema_remains_ordered_instance_data`; `draft_07_dependencies_schema_and_string_set_forms_are_context_aware`; `draft_07_dependencies_reject_invalid_shapes`; `draft_2020_12_legacy_keywords_remain_instance_data`; `ordered_schema_annotation_and_extension_arrays_remain_sensitive`; `schema_keyword_shaped_annotation_keys_remain_instance_data`; `draft_2020_12_object_items_traverses_nested_schema`; `draft_2020_12_array_items_is_malformed`; `draft_07_array_items_preserves_tuple_order`; `draft_07_additional_items_traverses_schema_context`; `draft_07_additional_items_without_tuple_items_traverses_schema_context`; `shared_single_schema_keywords_traverse_closed_dialect_context`; `shared_single_schema_keywords_reject_non_schema_shapes_with_closed_detail`; `nested_schema_dialect_is_rejected_in_every_shared_single_schema_keyword`; `draft_2020_12_dependent_required_property_arrays_are_canonical_string_sets`; `dependent_required_rejects_non_string_set_shapes`; `boolean_items_is_canonical_nested_schema`; `raw_schema_rejects_duplicate_keys`; independent exact counting vectors pin root depth, value nodes, decoded key/value strings, direct entries, raw bytes, and canonical bytes; exact-limit and limit-plus-one fixtures for every `McpContractLimitKind`; deep/wide input does not panic; `rg` API audit proving no public `from_serializable`, `serde_json::Value`, or typed-map evidence constructor |
| B-016 | `git diff` manifest check plus `rg` call-site audit proving no production consumer |

Cross-cutting mandatory runtime tests additionally prove:

- every owned role waits for pidfd registry commit before `GO`, while failed
  registration runs no workload and either reaps the gated direct child or
  retains the exact obligation;
- owner admission accepts eight, the ninth fails before work, and permits stay
  held until actual owner exit with an empty registry;
- exact limits are two pidfds and 28 non-pidfd slots per owner, 16 pidfds and
  320 post-`DescriptorsReady` retained descriptors globally. Before readiness
  one bootstrap child per owner may transiently inherit the process-wide table
  in addition to an admitted target and performs no workload. The active
  deadline bounds readiness waiting; exact rollback uses the cleanup deadline
  and retains the obligation and permit until reap. After readiness phase
  accounting freezes at most 12 simultaneous allowlisted child references (or
  target three plus observer five), with no foreign owner descriptors;
- cwd opens with `O_PATH | O_DIRECTORY | O_CLOEXEC` and no `O_NOFOLLOW`,
  including a search/execute-only directory and pathname replacement;
- x86_64 x32 dispatch fails before native classification, x86_64 `uselib` is
  denied as executable mapping, and aarch64 has no fabricated `uselib` entry;
- output overflow/read failure precedes exit classification; fatal delivered
  signals are reinjected and yield only `terminated_by_signal`, caught/ignored
  signals continue, direct `SIGKILL` is semantic only from `AwaitEntry` or
  `AwaitExit`, direct `SIGKILL` from `AwaitInitialExecExit` and illegal delivery
  transitions fail verification, and signal
  or nonzero outcomes do not also emit UTF-8/blank/grammar failures;
- success requires target reap, complete bounded streams, a passing post-reap
  checkpoint, and an empty registry, with no PGID, membership, negative-PID,
  post-reap-PID, or descendant-tree evidence;
- an unrelated same-session process continuously calling `setpgid` never
  enters the registry or evidence, is never signalled, and cannot affect
  success;
- launch/source/envelope/schema limits retain their exact and limit-plus-one
  vectors, and all failures omit raw paths, output, environment, and OS text.

Direct and `/usr/bin/env` shebang fixtures stop before interpreter or target
creation. macOS, other Unix, and Windows stop before cwd observation.
Cancellation drops the hosting Tokio runtime and proves the independent owner
continues exact registered-pidfd cleanup. Expected digests are independent
fixed vectors, never values generated by the helper under test.

## Handoff Notes

- PR #1859 remains the sole implementation PR and must be repaired on its
  original branch only after maintainers approve this packet and record
  `ready_to_implement`.
- The six-file packet is normative as a unit:
  `product.md`, `runtime-product.md`, `runtime-observation.md`,
  `runtime-supervision.md`, `tech.md`, and `tasks.md`.
- `runner_observed` strengthens evidence observation; it never changes
  repository, user, admin, system, runtime, or runner ownership.
- Base source locators are bounded at 4,096 UTF-8 bytes, complete derived
  locators at 8,259 bytes, exact source and raw envelope input at 2,097,152
  bytes, with checked arithmetic and closed precedence.
- Launch inputs are bounded at 65,536 exact Unix-byte or Windows-UTF-16 units;
  environment/setup collections and names are bounded at 1,024. Excluded or
  undeclared values are never read.
- The owner admits exactly eight fingerprints. Each has two pidfd slots and 28
  non-pidfd slots; post-ready global retained ceilings are 16 pidfds and 320
  descriptors. A pre-ready child may transiently inherit the process table;
  readiness timeout starts cleanup-deadline rollback and retains its obligation
  and permit until reap;
  after readiness one child has at most 12 allowlisted references and a
  post-exec target plus observer at most eight. No other concurrent roles
  exist. The permit remains held until the owner exits with an empty registry.
- Every child is gated until its pidfd plus reap obligation is registered.
  Pre-registration rollback may use only its exact still-unreaped positive PID;
  the capability child must prove validating/consuming `waitid(P_PIDFD)`
  before cwd. Exact-PID reap is allowed only when failed bootstrap capability
  detection leaves the child unreaped; a successful consuming wait forbids it.
  After success all registered-child cleanup uses only pidfds.
- Success proves registered obligations are empty and the guarded target
  executed no process-creation syscall. It does not claim descendant-tree
  emptiness. Anchors, PGIDs, membership stages, and `/proc` group scans are
  outside the design.
- Retained cwd uses `O_PATH | O_DIRECTORY | O_CLOEXEC` without
  `O_NOFOLLOW`; retained target execution remains fd-10
  `execveat(AT_EMPTY_PATH)`.
- Initial and retry supervision setup stages are exactly
  `working_directory_enter` and `trace_setup`.
- Output precedence is capture completion, then signal/nonzero, then
  zero-exit-only UTF-8/blank/grammar selection.
- Core schema dialect, raw-number preservation, MCP bounds, fingerprint digest,
  authorized 14-path manifest, no-lockfile, and producer-only constraints are
  unchanged.
