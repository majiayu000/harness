# Product Spec

## Linked Issue

GH-1771

complexity: high

## User Problem

Workflow-runtime prompts mix trusted and untrusted content without a
consistent boundary. Agent-authored values stored in `workflow.data`
(summaries, external-state strings, continuation payloads) are replayed into
later prompts as trusted orchestration state, so text originating in a
hostile issue body can gain instruction authority one turn after it was
correctly fenced. The default Claude agent profile runs with permissions
checks disabled, so any successful injection executes with the full tool
surface. Network egress control is delegated to an external proxy that
Harness does not ship or verify; the host tier has no egress control at all.

Operators need untrusted content to stay visibly untrusted for its whole
lifetime, agents to run with the least tool surface their activity needs,
and a network floor that Harness itself enforces or verifies.

## Goals

- Classify every field written into `workflow.data` by provenance: server-,
  agent-, or external-origin.
- Re-inject agent- and external-origin values into prompts only inside the
  established untrusted framing; keep server-origin values as today.
- Make the scoped tool profile the default for workflow-runtime agent
  spawns, with per-activity tool sets and an explicit, recorded opt-up to
  the full profile.
- Define an egress enforcement contract: deny-by-default allowlist on the
  host tier, and a bundled-proxy option so the container tier can reach its
  allowlist without unshipped infrastructure.
- Attach fencing obligations to a `harness.runtime.prompt_packet.v3`
  schema bump above GH1732's mandatory v2, so the declared schema states
  which obligations apply.
- Never render an unclassified field as trusted: legacy pre-sidecar fields
  degrade to fenced-untrusted with recorded evidence; post-sidecar
  unclassified writes are an error.

## Non-Goals

- Changing the sandbox filesystem policy (`.git` / `.harness` write-deny is
  unchanged).
- Adding isolation tiers; `IsolationTier::Microvm` remains unimplemented.
- Replacing or duplicating the GH1732 context-provenance manifest; this
  work consumes its trust taxonomy for workflow-state fields.
- Breaking prompt-packet consumers without a packet schema version bump.
- Scoping database or GitHub credentials (separate work; noted as adjacent).
- Treating repo-sourced `WORKFLOW.md` `prompt_template` as untrusted; it
  remains trusted-by-design for the repository's own workflows and is
  covered by provenance visibility only.

## User-Visible Behavior

1. **B-001:** Every write into `workflow.data` records a provenance class
   for the written field: `server` (derived by Harness from verified
   sources), `agent` (parsed from an activity result or other
   agent-authored output), or `external` (issue, comment, review, or
   webhook text). Provenance rides beside the data without changing the
   shapes existing consumers read.
2. **B-002:** Once this feature ships, every newly produced
   workflow-runtime prompt packet declares packet schema
   `harness.runtime.prompt_packet.v3`, which carries all v2 (GH1732)
   obligations plus fencing: fields classed `agent` or `external` appear
   only inside untrusted framing (the existing external-data fence or the
   repo-memory untrusted preamble contract). Fields classed `server`
   render byte-identically to v2. Historical v1 and v2 packets remain
   valid lower-evidence records and are never reinterpreted as v3.
3. **B-003:** Continuation context is always treated as agent-origin and is
   always fenced, regardless of stored provenance.
4. **B-004:** A v3-packet construction that encounters a `workflow.data`
   field with missing or unrecognized provenance never renders it as
   trusted. A field last written before the provenance sidecar existed
   (grandfathered by the sidecar migration marker) renders inside
   untrusted framing and records a degradation artifact naming the field.
   A field written after the sidecar exists with no classification is a
   writer defect and fails packet construction with a typed error.
5. **B-005:** Workflow-runtime Claude spawns default to a scoped tool
   profile derived from the activity policy. Read-class activities receive
   a read-only tool set; implementation-class activities receive the write
   set defined by their profile. The permissive full profile
   (`--dangerously-skip-permissions`) is used only when configuration
   explicitly opts that profile up, and the effective profile is recorded
   in the packet evidence.
6. **B-006:** An opted-up full profile is never inferred: absence of
   configuration yields the scoped default. Existing non-runtime callers
   that construct requests with `allowed_tools = None` outside the workflow
   runtime keep their current behavior.
7. **B-007:** Host-tier spawns honor a deny-by-default egress allowlist
   when egress enforcement is configured; a spawn requiring enforcement
   that the platform cannot provide is a visible dispatch error, not a
   silent full-network run.
8. **B-008:** Container-tier spawns can satisfy their allowlist through a
   Harness-bundled proxy; when a proxy is configured, Harness verifies the
   proxy is reachable and filtering before dispatching, and dispatch fails
   visibly when verification fails. The existing behavior — `--network
   none` when no allowlist+proxy pair is configured — is preserved.
9. **B-009:** Every enforcement decision (fenced field count, effective
   tool profile, egress mode and verification result) is observable in the
   runtime evidence for the job, so an operator can answer "what surface
   did this activity actually have" after the fact.
10. **B-010:** Server-origin fields, the container `--network none` path,
    and every historical v1/v2 packet keep their current behavior and
    recorded semantics. For in-flight workflows, the only rendering change
    on their first v3 packet is that agent-, external-, and grandfathered
    unclassified fields move inside untrusted framing; no workflow is
    blocked solely because it predates the sidecar.

## Acceptance Criteria

- [ ] Writer-side tests prove each reducer/worker write path that mutates
      `workflow.data` records the expected provenance class, including
      continuation payloads (`agent`) and snapshot-derived facts (`server`).
- [ ] Packet tests prove `agent`/`external` fields render only inside
      untrusted framing in v3 packets, byte-identical trusted rendering for
      `server` fields, and a typed error for unclassified fields.
- [ ] A regression test replays the two-turn attack: external text fenced
      on turn 1, stored via an agent summary, and proven fenced again on
      turn 2.
- [ ] Spawn tests prove the runtime default profile emits `--allowedTools`
      with the activity's tool set, that opt-up emits
      `--dangerously-skip-permissions` only with explicit configuration,
      and that the two flags remain mutually exclusive.
- [ ] Egress tests prove host-tier enforcement denies a non-allowlisted
      host and that unverifiable enforcement blocks dispatch with a typed
      error; container tests prove the bundled-proxy path and the preserved
      `--network none` fallback.
- [ ] Evidence tests prove profile, egress mode, and fencing counts are
      recorded per job.
- [ ] Grandfathering tests prove a pre-sidecar field renders fenced with a
      degradation artifact while a post-sidecar unclassified write fails
      construction; historical v1/v2 packet fixtures remain readable and
      unreinterpreted.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-004 and B-006: missing provenance is an error; missing profile configuration yields the scoped default. |
| Error and failure paths | Covered by B-004, B-007, B-008: enforcement failures are typed, visible, and block dispatch rather than degrading. |
| Authorization / permission | Core subject: B-005–B-008 narrow the default execution and network authority. |
| Concurrency / race / ordering | Provenance is written in the same store transaction as the data mutation; packet construction reads a single snapshot. |
| Retry / repetition / idempotency | Re-dispatch reconstructs the packet from stored data + provenance; fencing decisions are deterministic per snapshot. |
| Illegal state transitions | N/A directly; workflow transition rules unchanged. |
| Compatibility / migration | Covered by B-002, B-004, and B-010: historical v1/v2 packets keep their semantics; the v3 schema carries the new obligations; pre-sidecar fields are grandfathered to fenced-untrusted instead of blocking. |
| Degradation / fallback | Covered by B-007/B-008: no silent fallback from enforced to open network, no silent fallback from scoped to full profile. |
| Evidence and audit integrity | Covered by B-009. |
| Cancellation / interruption / partial completion | A dispatch blocked by enforcement leaves the job undispatched and retryable; no partial spawn. |

## Edge Cases

- An agent result echoes a server-derived value verbatim; the copy written
  from the agent result is classed `agent` (writer identity, not content,
  decides).
- A `workflow.data` field is overwritten across turns by writers of
  different classes; provenance always reflects the latest writer.
- Nested objects mix classes; provenance is per-field (JSON pointer), and
  an object containing any non-`server` member fences the non-`server`
  members only.
- Continuation context absent: no fence emitted, no error.
- Operator enables host-tier enforcement on a platform with no supported
  mechanism: dispatch error names the platform gap.
- Proxy verification succeeds but the proxy later dies mid-turn: the run
  fails on network errors like any outage; post-hoc evidence still shows
  verified-at-dispatch.
- An in-flight workflow whose packets were v2 is recovered/retried after
  the feature ships: its next packet is v3, its pre-sidecar fields render
  fenced with degradation artifacts, and it is not blocked.

## Rollout Notes

Two-step rollout aligned with GH1732's every-new-packet discipline. Step
one lands the provenance sidecar dark: writers classify, nothing renders
differently, and a sidecar migration marker records when classification
began (the grandfathering boundary). Step two bumps the packet schema to
v3, at which point every newly produced packet fences agent-, external-,
and grandfathered fields; there is no per-definition adoption window, so
declared schema and rendering behavior never diverge. Profile default
flips via configuration with a release-note callout; operators needing
the old behavior opt profiles up explicitly. Egress enforcement is opt-in
per tier configuration at first, with deny-by-default targeted once the
bundled proxy has soaked. Reverting step two pins packets back to v2
rendering without data migration; the provenance sidecar stays inert
under v2 and keeps accumulating classifications for a later re-flip.
