# syntax=docker/dockerfile:1.7
ARG NODE_BASE_IMAGE=docker.io/library/node:22-bookworm-slim@sha256:813a7480f28fdadac1f7f5c824bcdad435b5bc1322a5968bbbdef8d058f9dff4
FROM ${NODE_BASE_IMAGE}

ARG CODEX_CLI_PACKAGE=@openai/codex@0.142.5
ARG CLAUDE_CODE_PACKAGE=@anthropic-ai/claude-code@2.1.201

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        git \
        openssh-client \
        python3 \
        ripgrep \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g "${CODEX_CLI_PACKAGE}" "${CLAUDE_CODE_PACKAGE}" \
    && npm cache clean --force \
    && command -v codex \
    && command -v claude

RUN useradd --create-home --home-dir /home/harness --shell /bin/bash harness \
    && mkdir -p /workspace \
    && chown -R harness:harness /workspace /home/harness

ENV HOME=/home/harness
WORKDIR /workspace
USER harness
ENTRYPOINT []
