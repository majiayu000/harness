# Product Spec

## Linked Issue

GH-1732

complexity: high

## User Problem

The workflow runtime records the complete prompt packet and a SHA-256 digest,
but an operator still cannot answer which behavior-affecting inputs were
selected to construct it, in what order, or why. A digest proves packet
integrity; it does not distinguish workflow configuration, runtime profile,
retrieved memory, dynamic task input, or context that Harness cannot observe.

Repository inventory also cannot solve this problem because discovering a file
does not prove it was selected or loaded for a runtime activity.

The first implementation exposed three compatibility gaps after merge: valid
profile names could make provenance construction fail, the model-facing packet
still advertised the durable v2 schema, and omitted approval policy was
misclassified for runtimes that do not support that setting. The remediation
must correct those claims without weakening durable v2 evidence.

## Goals

- Record a redacted provenance manifest for sources the workflow runtime
  actually selects while constructing a prompt packet.
- Link provenance deterministically to the existing prompt packet and runtime
  job evidence.
- Preserve source identity, ordering, selection reason, digest, observation
  class, and coverage limitations.
- Make missing or unserializable required provenance a visible error.
- Avoid persisting raw memory payloads, secrets, or hidden model reasoning.
- Accept every profile name already accepted by runtime configuration while
  keeping provenance locators valid and deterministic.
- Keep durable packets on v2 while preserving the model-facing v1 packet
  contract for unchanged inputs.
- Distinguish an unobserved Codex default from a setting that is not applicable
  to the selected runtime.

## Non-Goals

- Discovering all repository or user-global Agent Stack components.
- Claiming that Codex, Claude, MCP, or another adapter loaded context that
  Harness cannot observe after process launch.
- Persisting full memory payloads a second time in provenance.
- Replacing the current prompt packet, packet digest, runtime event, or
  activity-result artifact.
- Recording tool execution or capability use; that belongs to ASC-010.
- Producing aggregate stack snapshots, promotion policy, or attestations.
- Adding a public protocol response DTO or changing an external API wire
  contract; provenance is an additive field in the private runtime prompt
  packet representation.

## User-Visible Behavior

1. **B-001:** Every newly produced workflow-runtime prompt packet declares
   packet schema `harness.runtime.prompt_packet.v2` and contains exactly one
   `context_provenance` object with schema
   `harness.runtime.context_provenance.v1`. A v2 packet with missing, blank, or
   unsupported provenance is invalid. The associated `runtime_prompt_packet`
   activity artifact declares the same packet-schema value from one shared
   constant. Historical v1 packets and artifacts remain valid lower-evidence
   records and are never interpreted as v2.
2. **B-002:** Provenance records only inputs selected by Harness while building
   that packet. Repository discovery alone never produces a selected, loaded,
   or runtime-observed provenance entry.
3. **B-003:** Each entry records a stable source ID, typed component kind,
   source scope/locator, lowercase SHA-256 digest, selection reason,
   zero-based order, observation class, selection state, and trust level using
   the ASC-001 model.
4. **B-004:** Runtime provenance records runtime kind, profile name, execution
   phase, effective max-turns, timeout, and stall timeout, plus final model,
   reasoning effort, sandbox, and approval policy after their
   profile/workflow/server fallbacks. This includes Claude's phase-sensitive
   model and effort selection when the profile omits explicit values. A setting
   whose effective value is resolved outside Harness — specifically a Codex
   approval policy the profile omits — is recorded as explicitly unobserved
   rather than given a fabricated final value. A setting unsupported by the
   selected runtime is recorded as not applicable, not unobserved. The final
   launch settings are computed once
   before packet construction and shared by packet construction and agent
   launch. The source is runtime-scoped, observation and trust are
   `runtime_observed`, selection state is `loaded`, and changing any recorded
   setting changes its digest.
5. **B-005:** Workflow provenance distinguishes the ordered central-base and
   repository `WORKFLOW.md` sources from the normalized effective document.
   Configured sources retain content digests and safe typed locators even when
   the effective document is merged; unsafe absolute paths are redacted, not
   misclassified as defaults. If neither source exists, an explicit runtime
   default entry is emitted instead of inventing a repository file.
6. **B-006:** Every repo-memory record selected into the packet produces one
   memory entry in the same order. It records durable record identity,
   evidence reference when present, estimated token count, and a digest of the
   exact redacted packet representation, but does not duplicate raw payload
   content in the provenance object. Observation is `runtime_observed` because
   Harness selected it, while trust remains `self_declared` because selection
   does not validate the memory claim.
7. **B-007:** When memory is enabled but retrieval fails, existing degradation
   evidence remains authoritative and provenance records no fabricated memory
   entry. The packet cannot claim selected memory that was not returned.
8. **B-008:** Dynamic workflow instance and command input remain visibly
   identified as per-invocation payload sections, not reusable context
   dependencies. For prompt tasks, the packet also records the durable prompt
   reference and SHA-256 of the exact task text before packet hashing; raw task
   text is not duplicated in provenance. These inputs are covered by the
   enclosing packet digest and are not misclassified as trusted context.
9. **B-009:** Provenance declares its coverage limitations. Context loaded
   independently by an agent CLI, MCP host, user-global configuration, or
   model provider is marked `not_observed_by_harness`; absence from the
   manifest is never presented as proof that such context did not exist.
10. **B-010:** Provenance contains no raw secret-bearing environment values,
    credential values, full memory payload copies, or model reasoning. Safe
    source locators and digests remain available for audit.
11. **B-011:** Entry ordering and serialization are deterministic. Rebuilding
    the same packet inputs produces identical provenance and packet digest;
    changing a recorded source or its order changes the digest.
12. **B-012:** Provenance construction or serialization failure aborts prompt
    preparation before agent execution and returns an error. Harness never
    records `RuntimePromptPrepared` with missing required provenance or
    silently substitutes an empty manifest.
13. **B-013:** `RuntimePromptPrepared` continues to persist the prompt packet
    and existing packet digest. Because provenance is nested in the packet,
    the event atomically links the runtime job, provenance entries, and digest
    without a second partially committed record. The activity artifact carries
    that same digest and the same packet-schema constant.
14. **B-014:** Existing prompt semantics, repo-memory selection, runtime
   profile selection, activity policy, activity-result status, and workflow
   state transitions remain unchanged. Durable packets and their activity
   artifacts retain schema `harness.runtime.prompt_packet.v2` and all required
   audit metadata. Before rendering for the agent, the model-facing clone
   removes audit-only fields and restores schema
   `harness.runtime.prompt_packet.v1`; for unchanged inputs its serialized
   packet section and complete prompt bytes are identical to the pre-v2
   rendering.
15. **B-015:** Every profile name accepted by `RuntimeProfile`, including names
   with spaces, slashes, Unicode, or UUID-shaped text, produces a valid,
   deterministic runtime provenance locator derived from the lowercase SHA-256
   of the exact UTF-8 profile-name bytes. The unhashed profile name remains
   unchanged in `resolved_runtime_settings.profile_name`; the locator does not
   expose or normalize it. Equal names produce equal locators, and unequal test
   names produce unequal locators.
16. **B-016:** Omitted approval policy resolves by runtime capability. Codex
   runtimes record `unobserved_agent_default`; Claude Code and Anthropic API
   record `not_applicable`; explicit approval policy remains rejected for every
   non-Codex runtime. `not_applicable` never implies that an agent-side default
   influenced launch.

## Acceptance Criteria

- [ ] Runtime profile, workflow document/defaults, and each selected repo-memory
      record produce validated ASC-001 provenance entries.
- [ ] Agent launch and provenance consume the same resolved model, reasoning,
      execution phase, sandbox, approval, and timeout values; provenance also
      records the effective max-turns already enforced by the workflow runtime.
- [ ] Claude fixtures prove explicit profile values take precedence and omitted
      model/effort values resolve from the same phase and server configuration
      used by agent launch.
- [ ] Central, repository, merged, and default workflow cases retain truthful
      source identities and content/effective digests without leaking unsafe
      absolute paths.
- [ ] Prompt-task fixtures prove that changing durable task text under the same
      reference changes the packet digest while raw text is not duplicated.
- [ ] The packet contains coverage declarations for independently loaded
      context that Harness cannot observe.
- [ ] Provenance omits raw secret values and duplicate memory payload content.
- [ ] Packet digest fixtures prove stability and sensitivity to source/order
      changes.
- [ ] Prompt preparation fails before execution when required provenance
      construction fails.
- [ ] Existing prompt, memory, activity-policy, and activity-result tests
      remain green without weakening assertions.
- [ ] Named test
      `arbitrary_profile_names_use_stable_hashed_locators_and_preserve_profile_name`
      proves B-015 for spaces, slashes, Unicode, and UUID-shaped names.
- [ ] Named test
      `model_facing_prompt_uses_v1_schema_while_durable_packet_remains_v2`
      proves the exact B-014 durable/model schema split and v1 prompt bytes.
- [ ] Named test
      `non_codex_omitted_approval_policy_is_not_applicable` proves B-016 for
      Claude Code and Anthropic API while the existing Codex case remains
      `unobserved_agent_default`.
- [ ] No database migration, new runtime event type, or external dependency is
      introduced.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001, B-005, B-007, B-009, and B-016. |
| Error and failure paths | Covered by B-007 and B-012. |
| Authorization / permission | Covered by B-009, B-010, and B-016; provenance observes no new authority, distinguishes unsupported policy, and exposes no credentials. |
| Concurrency / race / ordering | Covered by B-006, B-011, and B-013; provenance is built from the same immutable inputs before the existing event write. |
| Retry / repetition / idempotency | Covered by B-011 and B-013. |
| Illegal state transitions | N/A. Provenance does not change workflow state; ASC-001 validation rejects illegal evidence combinations. |
| Compatibility / migration | Covered by B-001, B-013, B-014, B-015, and B-016. |
| Degradation / fallback | Covered by B-007, B-009, and B-012; missing evidence is never success-shaped. |
| Evidence and audit integrity | Covered by B-002 through B-013, B-015, and B-016. |
| Cancellation / interruption / partial completion | Covered by B-012 and B-013; failure precedes agent execution and the existing event records packet plus provenance atomically. |

## Edge Cases

- Workflow defaults are active because no `WORKFLOW.md` exists.
- A central workflow base is active outside the repository.
- Central and repository workflow sources are merged.
- Memory is enabled with zero selected records.
- Two selected memory records contain equivalent redacted content but different
  durable identities.
- An adapter independently reads repository instructions after Harness creates
  the packet.
- A runtime profile has absent optional fields.
- A runtime profile name contains spaces, slashes, Unicode, or is a UUID.
- Claude Code or Anthropic API omits the unsupported approval-policy field.
- Server defaults resolve two otherwise identical runtime profiles to
  different effective model or sandbox settings.
- One durable prompt reference resolves to changed task text.
- Provenance serialization fails before `RuntimePromptPrepared`.
- A retry builds the same logical packet after a previous interrupted attempt.

## Rollout Notes

This emits version-2 prompt packets and leaves stored version-1 packets
unchanged. Consumers that only render the packet continue to treat it as a JSON
object. Evidence readers require provenance only for v2, treat v1 as
lower-evidence history, and never infer provenance for a packet lacking the
field. Agent prompt rendering continues to receive a v1-schema compatibility
clone; the v2 schema and evidence remain confined to the durable packet and
activity artifact.
