# Product Spec

## Linked Issue

GH-1730

complexity: medium

## User Problem

Harness can hash individual skills and runtime prompt packets, but it has no
shared contract for describing all components that can change agent behavior,
authority, or supplied context. Without that contract, inventory, runtime
provenance, stack diff, evaluation, and evidence features would each invent
their own component names, trust labels, and missing-data behavior.

Operators and downstream Harness subsystems need one strict, versioned model
that states what was observed without confusing repository discovery with
runtime use or local observation with trusted attestation.

## Goals

- Define the closed vocabulary and invariants for one Agent Stack component.
- Preserve the distinction between discovery, eligibility, selection, loading,
  and runtime observation.
- Represent source, digest, capability, freshness, and trust facts without
  untyped public payloads.
- Fail visibly on unsupported schemas and internally inconsistent evidence.
- Provide a stable foundation for ASC-002 through ASC-012 and ASC-018/ASC-019.

## Non-Goals

- Scanning repositories, global configuration, or executable installations.
- Computing aggregate stack snapshots, stable stack IDs, or diffs.
- Assigning risk scores or `PROMOTE` / `REVIEW` / `BLOCK` decisions.
- Recording runtime tool calls or granting capabilities.
- Replacing `CodeAgent`, `AgentAdapter`, `RuntimeKind`, `ContextItem`,
  `CapabilityToken`, or sandbox policy.
- Defining compatibility aliases for component kinds or serialized fields.

## User-Visible Behavior

1. **B-001:** Every serialized Agent Stack component declares the exact schema
   version `agent-stack-component/v0.1`. A missing, blank, or unsupported
   version is invalid and cannot be accepted as evidence.
2. **B-002:** Component kind is a closed snake_case vocabulary containing
   `instructions`, `skill`, `mcp_server`, `mcp_tool`, `hook`, `memory`,
   `policy`, `workflow`, `validation`, and `agent_runtime`. Unknown spellings,
   aliases, and case variants are rejected.
3. **B-003:** Every component has one canonical stable component ID derived
   exactly as `<source_scope>:<component_kind>:<source_locator>`, one typed
   source scope, and one non-empty source locator. IDs cannot use UUIDs,
   mutable display labels, per-scan values, or producer-specific aliases.
   Repository and user-global locators are portable paths relative to their
   declared scope root and cannot be absolute or escape with `..`. Runtime and
   runner locators use `<namespace>/<stable_key>` derived from a persisted,
   versioned configuration identity; they cannot be UUID-shaped or be derived
   from presentation text. Changing the key while the configured source
   identity is unchanged is a producer contract violation; a changed key is
   valid only when it identifies a genuinely different configured source.
4. **B-004:** Observation class is a closed vocabulary identifying the
   observer boundary: `repository_observed`, `runtime_observed`, or
   `runner_observed`. The class constrains the strongest admissible trust claim
   but does not by itself prove selection, loading, or execution. Repository
   observation may report repository-level selection, but never proves that a
   runtime loaded or used the component.
5. **B-005:** Selection state is exactly one of `discovered`, `eligible`,
   `selected`, `loaded`, or `observed`. A component may report only a state
   supported by its observation: repository-only evidence cannot report
   `loaded` or `observed`, and `observed` requires runtime- or runner-observed
   evidence.
6. **B-006:** Integrity is either absent or a validated lowercase SHA-256
   digest. Blank, malformed, mixed-case, or non-SHA-256 values are invalid.
   Absence remains distinct from a digest of empty content.
7. **B-007:** Capability identifiers use the closed initial vocabulary
   `destructive`, `secret_read`, `network`, `privileged`,
   `production_write`, `shell`, and `file_write`. This field states associated
   capability facts only; it does not state whether authority was declared,
   granted, or exercised.
8. **B-008:** Trust level is exactly one of `self_declared`,
   `repository_observed`, `runtime_observed`, or `runner_observed`. A component
   cannot claim a trust level stronger than its observation source supports.
   Stronger attestation and human-approval claims remain out of scope for this
   schema version.
9. **B-009:** Freshness is exactly one of `unknown`, `fresh`, `stale`, or
   `expired`. When no supported freshness fact exists, the value is `unknown`;
   callers do not infer freshness from file names, enumeration order, or the
   current clock.
10. **B-010:** Optional unavailable facts remain absent. Empty strings, sentinel
    names such as `unknown-component`, fabricated aliases, and zero digests may
    not substitute for missing evidence.
11. **B-011:** Components serialize deterministically with snake_case field
    names, reject unknown public fields, and round-trip without changing their
    semantic values. Within one component, capabilities are canonicalized in
    lexicographic order by their exact snake_case wire spelling. Ordering
    multiple components and canonicalizing aggregate snapshots remain
    responsibilities of the later snapshot contract.
12. **B-012:** Introducing the model does not change existing agent execution,
    prompt construction, skill injection, capability enforcement, persisted
    workflow data, or public wire behavior outside the new schema.

## Acceptance Criteria

- [ ] Public Rust types represent every field and closed vocabulary in
      B-001 through B-010 without `serde_json::Value` or `Any`-like public
      escape hatches.
- [ ] The public JSON parser preserves whether input failed JSON syntax or a
      typed validation invariant; validation errors distinguish unsupported
      version, invalid or unstable identity, invalid source locator, invalid
      digest, illegal observation/selection combinations, and trust escalation.
- [ ] Positive fixtures round-trip every component kind, observation class,
      selection state, trust level, freshness value, and capability.
- [ ] Negative fixtures prove unknown fields, aliases, malformed digests,
      traversal locators, explicit `null` integrity, missing required values,
      and impossible evidence combinations fail.
- [ ] Existing `Capability`, `ContextItem`, `CapabilityToken`, and
      `RuntimeKind` types and behavior remain unchanged.
- [ ] The implementation adds no external dependency and no persistence
      migration.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001, B-003, B-006, and B-010. |
| Error and failure paths | Covered by B-001 through B-011; invalid evidence returns a typed error. |
| Authorization / permission | Covered by B-007 and B-012. The model describes capability association but grants no authority. |
| Concurrency / race / ordering | N/A. This issue defines immutable value validation and no shared mutable state. |
| Retry / repetition / idempotency | Covered by B-011; repeated validation and serialization preserve semantic values. |
| Illegal state transitions | Covered by B-005. The schema rejects impossible observation/selection combinations; it does not manage a mutable lifecycle. |
| Compatibility / migration | Covered by B-001, B-002, B-011, and B-012. |
| Degradation / fallback | Covered by B-010; missing evidence cannot silently degrade into invented success-shaped values. |
| Evidence and audit integrity | Covered by B-004 through B-010. |
| Cancellation / interruption / partial completion | N/A. Validation is synchronous and has no partial persistence. |

## Edge Cases

- A repository component uses an absolute locator or `../` traversal.
- A component has an empty digest rather than no digest.
- Repository discovery labels a component `loaded`.
- A runtime observation claims `runner_observed` trust.
- A future producer emits a new component kind to a v0.1 reader.
- A component has no freshness evidence.
- A non-repository producer supplies a UUID, display label, reserved missing
  sentinel, or newly generated per-scan value as its source locator.
- Capabilities are present but no declared/granted/observed evidence class has
  yet been attached by the later capability-evidence contract.

## Rollout Notes

This is an additive model with no current producers or consumers, so it needs
no migration or feature flag. Later issues must depend on the published schema
rather than copying its enums. If the contract is rolled back before those
consumers land, removing the module has no runtime effect.
