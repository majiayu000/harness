# GH-1733 — Runtime and MCP Fingerprints

Status: deferred.

Stable ID: `ASC-004`.

## Decision

Harness does not currently have a production consumer for runtime or MCP
fingerprints. The initial implementation from PR #1859 exposed an unused public
execution API, and the subsequent strict remediation expanded into process
supervision, executable-race containment, and cross-platform lifecycle policy.
That cost is not justified without a concrete consumer and threat model.

The #1859 production API is removed. PR #1912 and its branch remain research
material; they are not an implementation baseline or an approved contract.

## Problem Worth Preserving

When Agent Stack comparison becomes a product requirement, file inventories
alone will not identify changes to the selected agent executable or advertised
MCP tool contracts. A future fingerprint may provide diagnostic evidence for
those changes.

## Restart Gates

Work must not restart until all of the following are named and approved:

1. A production call path that consumes the fingerprint.
2. A storage owner and retention boundary, or an explicit decision not to
   persist the evidence.
3. A user-visible decision or workflow that changes because of the evidence.
4. A threat model stating whether the fingerprint is diagnostic evidence or a
   security boundary.
5. Supported platforms and an operational budget for collection.

ASC-005 existing as a backlog item is not sufficient. Its actual call path,
storage location, and user need must exist first.

## Future MVP Boundary

If the restart gates are satisfied, begin with only:

- explicitly configured executable paths;
- normalized version evidence;
- a stable digest of the exact MCP contract fields required by the consumer;
- explicit, typed collection failures;
- bounded input and output sizes;
- secret-free inputs and persisted evidence.

## Non-Goals for the MVP

- Discovering arbitrary executables through `PATH`.
- Claiming that local observation proves executable authenticity.
- Treating a digest as a signature or trust decision.
- General-purpose command execution or caller-supplied version arguments.
- `ptrace`, pidfds, syscall filtering, or a custom process supervisor.
- Capturing credentials, setup secrets, or complete environments.
- Supporting platforms that the production consumer does not require.

If the approved threat model later requires a stronger execution boundary,
that boundary must be designed as its own capability with its own consumers and
security review. It must not be hidden inside a diagnostic fingerprint helper.

## Done for the Deferred State

- The unused #1859 public production APIs are absent from `main`.
- GH-1733 and its dependent ASC-005 work are not marked ready.
- This decision record is the maintained requirement until the restart gates
  are met.
