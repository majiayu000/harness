# Product Spec

## Linked Issue

GH-1450

## User Problem

Agents run on the operator's host under OS sandboxing (Seatbelt/Landlock)
with host network and operator credentials. Issue bodies and PR comments are
untrusted input; a prompt-injected agent can currently reach the network and
act with the operator's GitHub token. There is no way to demand stronger
isolation for untrusted work without moving off harness entirely.

## Goals

- Introduce an isolation tier axis: `host` (today) / `container` /
  `microvm` (future), selectable per project and per intake trust class.
- Route untrusted-origin tasks to the stricter tier automatically.
- Scope credentials per tier (short-lived, repo-scoped tokens in container).

## Non-Goals

- Firecracker/microVM engine implementation (schema reserves the tier only).
- Multi-tenant hosting of third-party users.
- Changing defaults for existing trusted single-operator projects.
- A cloud control plane.

## Behavior Invariants

1. Isolation tier is declared in config per project, with an optional
   override map per intake trust class (e.g. non-collaborator issue author →
   `container`).
2. Startup validates that every configured tier is actually available on the
   host; an unavailable required tier is surfaced as degraded health at
   startup and the affected intake is refused, never silently downgraded.
3. A `container` task runs with: ephemeral per-task workspace, network
   egress allowlist, and a short-lived repo-scoped credential — never the
   operator's full token.
4. The chosen tier and the reason (trust-class rule that fired) are recorded
   in run evidence for every task.
5. Dispatch-time tier resolution is deterministic: same task metadata + same
   config → same tier.
6. Container teardown removes workspace and credentials at terminal state,
   including on crash/cancel (reaper covers orphans).
7. End-to-end issue→PR flow behaves identically across tiers from the
   workflow runtime's perspective (same activities, same evidence classes).

## Acceptance Criteria

- [ ] Config schema with per-project tier + trust-class overrides; startup
      availability validation with degraded-health surfacing.
- [ ] Container tier completes a full issue→PR workflow with network
      allowlist and scoped credential.
- [ ] Untrusted-author fixture maps to container automatically; tier decision
      visible in run evidence.
- [ ] Unavailable-tier dispatch fails visibly (health/status), no fallback.

## Edge Cases

- Docker daemon dies mid-task — task fails with an infrastructure failure
  class; retry policy applies; health reflects tier unavailability.
- Repo requires host-only tooling (e.g. macOS codesigning) but rule says
  container — task refused with an explicit conflict reason for the operator.
- Credential minting fails — dispatch refused before any agent starts.
- Author trust changes mid-flight (collaborator added) — tier decision is
  fixed at dispatch; re-evaluated only on new dispatches.

## Rollout Notes

Additive config; absent config means `host` everywhere (today's behavior).
Recommend first enabling container tier for one public repo's
non-collaborator issues. Requires documenting the Docker image contract
(agent CLI availability inside the image).
