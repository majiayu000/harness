# Harness Agent Image

This is the reference container-tier image for Harness agent spawns. It includes
the Codex and Claude CLIs plus basic repository tools.

Current pinned inputs:

- Base image: `docker.io/library/node:22-bookworm-slim@sha256:813a7480f28fdadac1f7f5c824bcdad435b5bc1322a5968bbbdef8d058f9dff4`
- Codex CLI package: `@openai/codex@0.142.5`
- Claude Code package: `@anthropic-ai/claude-code@2.1.201`

Build and verify locally:

```bash
scripts/verify-agent-container-image.sh
```

Publish and pin an operator image:

```bash
docker build -f docker/agent/Dockerfile -t ghcr.io/OWNER/harness-agent:2026-07-04 docker/agent
docker push ghcr.io/OWNER/harness-agent:2026-07-04
docker buildx imagetools inspect ghcr.io/OWNER/harness-agent:2026-07-04
```

Use the reported `Digest:` value in runtime config:

```bash
export HARNESS_AGENT_CONTAINER_IMAGE=ghcr.io/OWNER/harness-agent@sha256:...
scripts/verify-agent-container-image.sh
```
