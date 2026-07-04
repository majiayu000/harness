# Container Tier Operator Guide

Harness can route untrusted GitHub issue intake to the `container` isolation
tier. The container tier runs the agent CLI through Docker with only the task
workspace mounted, no inherited operator secrets, and the scoped GitHub token
mapped to `GITHUB_TOKEN` and `GH_TOKEN` inside the container.

## Build The Agent Image

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

## Enable Container Routing

Set an isolation rule for untrusted intake. Keep trusted work on `host` unless a
project explicitly needs the stronger tier for all tasks.

```toml
[isolation]
default_tier = "host"
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

Start the server with the pinned image in the environment:

```bash
export HARNESS_AGENT_CONTAINER_IMAGE=ghcr.io/OWNER/harness-agent@sha256:...
harness --config harness.toml serve
```

If container tasks need network access, set an explicit egress proxy:

```bash
export HARNESS_AGENT_EGRESS_PROXY=http://127.0.0.1:8080
```

Without `HARNESS_AGENT_EGRESS_PROXY`, Harness keeps the container network closed
even when an allowlist is configured. The allowlist is still passed into the
container as `HARNESS_AGENT_EGRESS_ALLOWLIST` for proxy-side enforcement.

## Health And Refusal Behavior

On startup, Harness probes Docker. If a configured rule requires `container`
and Docker is unavailable, health reports the `isolation` subsystem as degraded.
Dispatch refuses matching untrusted intake instead of silently downgrading it to
`host`.

Check health before enabling the rule broadly:

```bash
curl -s http://127.0.0.1:9800/health | python3 -m json.tool
```

Roll out to one public repository first. Verify that a non-collaborator issue
resolves to `container`, that the workspace mount is `/workspace`, and that the
container environment exposes only the scoped token names needed by GitHub CLI
or API calls.
