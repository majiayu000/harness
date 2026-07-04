#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${HARNESS_AGENT_CONTAINER_IMAGE:-}"
TMP_DIR="$(mktemp -d)"
WORKSPACE=""

cleanup() {
    rm -rf "$TMP_DIR"
    if [[ -n "$WORKSPACE" ]]; then
        rm -rf "$WORKSPACE"
    fi
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
    echo "docker is required to verify the agent container image" >&2
    exit 127
fi

if [[ -z "$IMAGE" ]]; then
    IID_FILE="$TMP_DIR/image.iid"
    docker build \
        --iidfile "$IID_FILE" \
        -f "$ROOT/docker/agent/Dockerfile" \
        -t harness-agent:fixture \
        "$ROOT/docker/agent"
    IMAGE="$(cat "$IID_FILE")"
else
    case "$IMAGE" in
        *@sha256:*) docker pull "$IMAGE" ;;
        sha256:*) ;;
        *)
            echo "HARNESS_AGENT_CONTAINER_IMAGE must be digest-pinned, for example ghcr.io/OWNER/harness-agent@sha256:..." >&2
            exit 2
            ;;
    esac
fi

WORKSPACE="$(mktemp -d "$ROOT/.agent-container-fixture.XXXXXX")"
printf "fixture workspace\n" >"$WORKSPACE/README.md"

docker run \
    --rm \
    --workdir /workspace \
    --mount "type=bind,src=$WORKSPACE,dst=/workspace" \
    --network none \
    --env GITHUB_TOKEN=fixture-scoped-token \
    --env GH_TOKEN=fixture-scoped-token \
    "$IMAGE" \
    sh -lc '
        set -eu
        test "$(pwd)" = "/workspace"
        test -f README.md
        command -v codex >/dev/null
        command -v claude >/dev/null
        test "${GITHUB_TOKEN:-}" = "fixture-scoped-token"
        test "${GH_TOKEN:-}" = "fixture-scoped-token"
        if env | grep -E "^(ANTHROPIC_API_KEY|OPENAI_API_KEY|HARNESS_SCOPED_GITHUB_TOKEN)=" >/dev/null; then
            echo "unexpected operator secret exposed in fixture" >&2
            exit 1
        fi
    '

echo "agent container fixture passed: $IMAGE"
