# syntax=docker/dockerfile:1.7
FROM docker.io/library/python:3.13-alpine@sha256:399babc8b49529dabfd9c922f2b5eea81d611e4512e3ed250d75bd2e7683f4b0

RUN addgroup -S harness-proxy \
    && adduser -S -G harness-proxy -H -h /nonexistent harness-proxy

COPY --chown=harness-proxy:harness-proxy proxy.py /opt/harness-egress-proxy/proxy.py

USER harness-proxy
EXPOSE 8080
HEALTHCHECK --interval=2s --timeout=2s --start-period=1s --retries=5 \
    CMD ["python3", "/opt/harness-egress-proxy/proxy.py", "--canary", "127.0.0.1:8080"]
ENTRYPOINT ["python3", "/opt/harness-egress-proxy/proxy.py"]
