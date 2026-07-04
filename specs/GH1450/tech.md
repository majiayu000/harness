# Tech Spec

## Linked Issue

GH-1450

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Sandbox engines | `crates/harness-sandbox/src/lib.rs` | `SandboxEngine::{Seatbelt, Landlock, None}`, `wrap_command` | Extend with tier concept above OS-sandbox layer |
| Agent spawn | `crates/harness-agents/src/{claude,codex}.rs` | Spawns CLIs as host processes | Container tier wraps the spawn |
| Intake | `crates/harness-server/src/intake/github_issues.rs`, webhook path | Ingests issues without author-trust classification | Trust-class source |
| Config | `crates/harness-core/src/config/` (agents.rs `sandbox_mode`, workflow.rs) | Per-agent sandbox_mode string | Tier schema home |
| Workspace lifecycle | worktree admission per `WORKFLOW.md`; reaper (`scripts/pg-orphan-cleanup.sh`, runtime cleanup) | Host worktrees, cleanup on terminal | Container workspace lifecycle mirrors this |

## Proposed Design

1. **Config schema** — `[isolation]` per project: `default_tier = "host"`,
   `[[isolation.rules]] trust = "non_collaborator", tier = "container"`,
   `network_allowlist = ["github.com", "api.anthropic.com", ...]`.
   `microvm` accepted by the parser but rejected at startup as unimplemented
   (explicit error, reserving the name).
2. **Trust classification** — at intake, classify author via existing GitHub
   metadata (author_association on issue/PR payloads): collaborator/member/
   owner → trusted; others → non_collaborator. Stored on the workflow row.
3. **Tier resolution** — pure function (task metadata + config → tier +
   reason), executed at dispatch, persisted in run evidence.
4. **Container runner** — a `ContainerSpawn` wrapper in harness-agents: runs
   the agent CLI inside a per-task ephemeral container (OCI via docker CLI
   invocation from the runner layer, not from crates that ban subprocess
   git/gh — spawning agents is already this layer's job): mounts the task
   workspace only, applies network allowlist (docker network + proxy env),
   injects scoped credential, no host home mount.
5. **Scoped credentials** — mint GitHub installation/fine-grained token
   scoped to the single repo with contents+pull_requests write; TTL ≤ task
   timeout; revoke at teardown (best-effort + short TTL as backstop).
6. **Availability probe** — startup checks docker availability when any rule
   demands container; failure → degraded health entry + refusal of matching
   intake (aligned with GH-1441's validate-at-startup pattern).

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P2 no silent downgrade | startup probe + dispatch guard | integration test: rule demands container, docker absent → dispatch refused, health degraded |
| P3 scoped credential | credential minting | test: container env contains scoped token, not operator token |
| P5 deterministic resolution | tier resolution fn | unit test: fixture matrix → stable tiers |
| P6 teardown | container lifecycle + reaper | integration test: cancel mid-run → no container, no credential |

## Data Flow

Intake payload (author_association) → trust class on workflow row →
dispatch-time tier resolution → container spawn (workspace mount, allowlist,
scoped token) → normal activity evidence → teardown (container rm, token
revoke).

## Alternatives Considered

- Jump straight to microVMs — rejected for v1: docker covers the credential
  and filesystem blast radius on a single-operator deployment; microvm tier
  name reserved.
- Egress proxy only (no container) — rejected: does not scope filesystem or
  credentials.
- Per-task GitHub App installation tokens vs PAT — App tokens preferred;
  fallback documented if the deployment uses a PAT (feature requires App).

## Risks

- Security: docker is not a hard security boundary vs kernel exploits —
  documented; microvm tier is the escalation path. Allowlist bypass via DNS —
  pin proxy + block direct egress.
- Compatibility: agent CLIs must exist in the image — publish a reference
  Dockerfile; image digest pinned in config.
- Performance: container start adds seconds per task — acceptable for
  untrusted tier only; host tier unchanged.
- Maintenance: two spawn paths — shared spawn contract trait with
  host/container implementations, both covered by the same contract tests.

## Test Plan

- [ ] Unit tests: tier resolution matrix, config parsing (including microvm rejection), allowlist construction.
- [ ] Integration tests: container issue→PR fixture flow; refusal paths (docker absent, credential mint failure); teardown on cancel.
- [ ] Manual verification: non-collaborator issue on a public repo runs in container; verify no operator token inside container env.

## Rollback Plan

Remove/disable `[isolation]` config → all dispatch resolves to host tier
(exact current behavior). Container runner is unreachable without config.
