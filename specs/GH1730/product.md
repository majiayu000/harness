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
   aliases, and case variants are rejected. The normative role table below
   determines classification from an explicit typed registration or discovery
   surface; producers do not infer kind from a file name, extension, or content.
   One backing source explicitly bound to multiple roles represents distinct
   components, while an untyped or ambiguous single-role discovery fails
   closed.
3. **B-003:** Every component has one canonical stable component ID derived
   exactly as `<source_scope>:<component_kind>:<source_locator>`, one typed
   source scope from `repository`, `user_global`, `admin`, `system`, `runtime`,
   or `runner`, and one non-empty source locator. IDs cannot use UUIDs, mutable
   display labels, per-scan values, lossy path conversions, or
   producer-specific aliases. Repository and admin locators are lossless UTF-8
   portable paths relative to their fixed roots. User-global locators use
   `<root_namespace>/<portable_relative_path>` and select one root by the
   contract's fixed precedence when roots overlap under its lexical comparison.
   An absent or relative `XDG_CONFIG_HOME` is ignored in favor of an absolute
   `HOME/.config/harness` fallback; the XDG namespace is unavailable only when
   neither input can produce an absolute root.
   Windows drive-letter casing is canonical-equivalent; other case-varied path
   segments and symlink aliases remain distinct logical sources. v0.1 accepts at
   most one configured-user root and rejects ambiguous configuration.
   System, runtime, and runner locators use a snake_case namespace followed by
   a stable, case-preserving logical path that losslessly preserves identifiers
   containing `_`, `-`, and `.`. They reject reserved sentinel, UUID, and
   display-label wire shapes in every segment. Source scope identifies where a
   component originates; observation class separately identifies who observed
   it, so stronger observation never rewrites component identity. Producers
   must derive logical keys from persisted, versioned configuration identity
   rather than presentation or per-scan data. The environment-independent wire
   parser validates syntax but does not attest producer provenance; later
   producers prove their mapping, and ASC-005 detects unauthorized
   cross-snapshot identity changes.
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
   When present, it hashes the exact source bytes: raw file bytes for a file or
   the exact embedded payload bytes for a built-in. No decoding, newline, BOM,
   Unicode, frontmatter, metadata, or multi-file normalization is implicit.
   A logical, structured, or multi-file source without a versioned canonical
   byte encoding omits integrity. Absence remains distinct from the standard
   SHA-256 digest of empty content.
7. **B-007:** Capability identifiers use the closed initial vocabulary
   `destructive`, `secret_read`, `network`, `privileged`,
   `production_write`, `shell`, and `file_write`. This field states associated
   capability facts only; it does not state whether authority was declared,
   granted, or exercised.
8. **B-008:** Trust level is exactly one of `self_declared`,
   `repository_observed`, `runtime_observed`, or `runner_observed`. A component
   uses the exact linear trust order `self_declared` <
   `repository_observed` < `runtime_observed` < `runner_observed` and cannot
   claim a level stronger than its observation source supports. Repository
   observation accepts the first two levels, runtime observation accepts the
   first three, and runner observation accepts all four. Stronger attestation
   and human-approval claims remain out of scope for this schema version.
9. **B-009:** Freshness is exactly one of `unknown`, `fresh`, `stale`, or
   `expired` and describes evidence freshness, not skill usage health. `fresh`
   requires a direct source read or probe in the current observation; `stale`
   identifies a cached prior observation not revalidated in the current one;
   `expired` requires authoritative invalidation or an explicit producer
   validity deadline for which `observation_time >= valid_until`; and `unknown`
   means none of those facts exists. Precedence is explicit expiry, current
   observation, cached prior observation, then unknown. Callers do not infer
   this value from file names, enumeration order, the current clock alone, or
   the existing skill `FreshnessClass`.
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

### Component Kind Semantics

| Kind | Normative typed role |
| --- | --- |
| `instructions` | Agent directive content not registered as a skill or policy |
| `skill` | A named reusable unit registered with a skill registry |
| `mcp_server` | An MCP server connection or process definition |
| `mcp_tool` | One tool definition advertised by an identified MCP server |
| `hook` | An action bound to an explicit lifecycle hook slot |
| `memory` | Content retained and recalled across agent invocations |
| `policy` | A constraint that decides allow, deny, required, or routing behavior |
| `workflow` | A multi-step orchestration definition |
| `validation` | A registered check that emits validation or pass/fail evidence |
| `agent_runtime` | The adapter, executable, or profile that starts an agent |

## Acceptance Criteria

- [ ] Public Rust types represent every field and closed vocabulary in
      B-001 through B-010 without `serde_json::Value` or `Any`-like public
      escape hatches.
- [ ] The public JSON parser preserves whether input failed JSON syntax or a
      typed validation invariant; validation errors distinguish unsupported
      version, noncanonical component identity, invalid source locator, invalid
      digest, illegal observation/selection combinations, and trust escalation.
- [ ] Positive fixtures round-trip every component kind, observation class,
      source scope, user-global root, selection state, trust level, freshness
      value, and capability.
- [ ] Core fixtures prove the closed kind vocabulary and that explicit
      multi-role bindings produce distinct component IDs. Producer handoff
      requirements assign typed-role classification and fail-closed handling
      for untyped or ambiguous discovery to ASC-002, ASC-003, and ASC-004.
- [ ] Integrity fixtures prove exact-byte hashing, byte-level distinctions, the
      empty-content digest, and optional-field behavior. Producer handoff
      requirements prohibit integrity when canonical source bytes do not exist.
- [ ] Core fixtures prove freshness evidence precedence and the exact deadline
      boundary. ASC-002 handoff requirements prohibit automatic mapping from
      skill usage freshness.
- [ ] Negative fixtures prove unknown fields, aliases, malformed digests,
      traversal and NUL-containing locators, explicit `null` integrity, missing
      required values, non-UTF-8 path inputs, and impossible evidence
      combinations fail.
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
- A backing file is explicitly bound to multiple typed roles.
- A producer has an artifact but no typed role from which to classify it.
- A component has no freshness evidence.
- A cached observation has no current source read, or an explicit validity
  deadline has ended.
- Two byte sequences differ only by LF/CRLF, BOM, or Unicode encoding.
- A non-repository producer supplies a UUID, display label, reserved missing
  sentinel, or newly generated per-scan value as its source locator.
- Two user-global producers observe the same root but choose different base
  directories instead of using its canonical root namespace.
- XDG and platform configuration roots expand to the same or overlapping path.
- A producer discovers a non-UTF-8 path or an untyped custom discovery root.
- An admin or built-in component is mislabeled as user-global or repository
  evidence.
- A repository component is observed at runtime and incorrectly receives a new
  runtime-scoped component ID.
- Capabilities are present but no declared/granted/observed evidence class has
  yet been attached by the later capability-evidence contract.

## Rollout Notes

This is an additive model with no current producers or consumers, so it needs
no migration or feature flag. Later issues must depend on the published schema
rather than copying its enums. If the contract is rolled back before those
consumers land, removing the module has no runtime effect.
