# Container Tier Operator Guide

Harness can route untrusted GitHub issue intake to the `container` isolation
tier. The container tier runs the agent CLI through Docker with only the task
workspace mounted and no ambient inherited operator secrets. Harness maps the
scoped GitHub token to `GITHUB_TOKEN` and `GH_TOKEN` inside the container. For
Claude, it also forwards the explicitly configured `ANTHROPIC_API_KEY` provider
credential by environment variable name, never as a value in Docker arguments.

## Build The Images

Use the reference image in `docker/agent/Dockerfile`. The Dockerfile pins the
Node base image by digest and pins the default Codex and Claude CLI packages.

```bash
scripts/verify-agent-container-image.sh
```

That command builds the image, runs the fixture by immutable local image ID, and
checks that:

- `/workspace` is the mounted task workspace.
- `codex` and `claude` are present.
- only the scoped GitHub token names are present.
- the container can run with `--network none`.

Publish the image and configure Harness with a registry digest, not a mutable
tag:

```bash
docker build -f docker/agent/Dockerfile -t ghcr.io/OWNER/harness-agent:2026-07-04 docker/agent
docker push ghcr.io/OWNER/harness-agent:2026-07-04
docker buildx imagetools inspect ghcr.io/OWNER/harness-agent:2026-07-04
export HARNESS_AGENT_CONTAINER_IMAGE=ghcr.io/OWNER/harness-agent@sha256:...
```

Re-run the fixture against the published digest:

```bash
HARNESS_AGENT_CONTAINER_IMAGE=ghcr.io/OWNER/harness-agent@sha256:... \
  scripts/verify-agent-container-image.sh
```

Build and publish the bundled allowlist proxy separately. Its base image is
digest-pinned, it runs as a non-root user, and it accepts exact DNS hostnames
only. Use an immutable registry digest in production:

```bash
docker build -f docker/egress-proxy/Dockerfile \
  -t ghcr.io/OWNER/harness-egress-proxy:2026-08-09 docker/egress-proxy
docker push ghcr.io/OWNER/harness-egress-proxy:2026-08-09
docker buildx imagetools inspect ghcr.io/OWNER/harness-egress-proxy:2026-08-09
export HARNESS_AGENT_EGRESS_PROXY_IMAGE=ghcr.io/OWNER/harness-egress-proxy@sha256:...
```

## Enable Container Routing

Set an isolation rule for untrusted intake. The example uses `container` as the
default because its non-empty allowlist must also work on Linux; macOS operators
may use `host` for trusted work.

```toml
[isolation]
default_tier = "container"
network_allowlist = [
  "github.com",
  "api.github.com",
  "api.openai.com",
  "api.anthropic.com",
]

[[isolation.rules]]
trust = "non_collaborator"
tier = "container"
```

Start the server with both pinned images in the environment:

```bash
export HARNESS_AGENT_CONTAINER_IMAGE=ghcr.io/OWNER/harness-agent@sha256:...
export HARNESS_AGENT_EGRESS_PROXY_IMAGE=ghcr.io/OWNER/harness-egress-proxy@sha256:...
export ANTHROPIC_API_KEY=sk-ant-...
harness --config harness.toml serve
```

The `ANTHROPIC_API_KEY` line is required when the selected container agent is
Claude and it does not have another authentication mechanism provisioned in
the image. Harness authorizes only that provider key for Claude container
spawns; unrelated operator credentials remain filtered.

`network_allowlist` is an exact-host allowlist. Harness starts one bundled
proxy container per agent and puts the agent on a unique internal Docker
network. The proxy alone is also attached to Docker's bridge network. The
agent therefore cannot bypass the proxy by ignoring `HTTP_PROXY`.

The list governs the whole CLI process, including model-provider requests and
tool subprocesses. Include the selected provider's required endpoints; Harness
does not add an implicit control-plane bypass that shell tools could reuse.

- Scoped mode with an empty allowlist has no network access.
- Any non-empty allowlist uses the first-party proxy, including when the tool
  capability profile is `full`.
- Only explicit `capability_profile = "full"` with an empty allowlist keeps
  unrestricted networking.

`HARNESS_AGENT_EGRESS_PROXY` is no longer accepted. Harness refuses that
legacy external-proxy configuration because it cannot prove enforcement.

## Health And Refusal Behavior

On startup, Harness probes Docker. If a configured rule requires `container`
and Docker is unavailable, health reports the `isolation` subsystem as degraded.
Dispatch refuses matching untrusted intake instead of silently downgrading it to
`host`.

For allowlisted dispatches, Harness also waits for the proxy image healthcheck.
Container dispatch starts with an in-container canary request to a deliberately
non-allowlisted hostname and requires a `403` response before the agent command
runs. A missing image, unhealthy proxy, failed canary, or missing proxy route is
a spawn error; Harness never falls back to open networking.

On macOS host isolation, scoped allowlisted agents are restricted by Seatbelt to
the proxy's loopback port. On Linux host isolation, deny-all networking is
supported, but proxy-only host networking is rejected because Landlock and
bubblewrap cannot express that boundary safely. Use the container tier for
Linux tasks that need allowlisted network access. The specific Linux
combination `danger-full-access` plus scoped deny-all networking requires
Bubblewrap even when `harness-landlock` is installed, because the Landlock
helper has no network-only mode. Startup health reports the host tier
unavailable when that requirement is unmet, and matching dispatches fail
closed.

Check health before enabling the rule broadly:

```bash
curl -s http://127.0.0.1:9800/health | python3 -m json.tool
```

Roll out to one public repository first. Verify that a non-collaborator issue
resolves to `container`, that the workspace mount is `/workspace`, and that the
container environment exposes only the scoped token names needed by GitHub CLI
or API calls.
