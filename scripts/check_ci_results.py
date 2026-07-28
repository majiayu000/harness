#!/usr/bin/env python3
"""Fail closed unless every GitHub Actions job has its permitted result."""

from __future__ import annotations

import os
import sys
from collections.abc import Mapping, Sequence


KNOWN_RESULTS = {"success", "skipped", "failure", "cancelled"}
RESULT_ENV_PREFIX = "HARNESS_CI_RESULT_"
EXPECTED_RESULT_ENV = {
    "changed": f"{RESULT_ENV_PREFIX}CHANGED",
    "storage-legacy-openers": f"{RESULT_ENV_PREFIX}STORAGE_LEGACY_OPENERS",
    "repository-checks": f"{RESULT_ENV_PREFIX}REPOSITORY_CHECKS",
    "fmt": f"{RESULT_ENV_PREFIX}FMT",
    "web-build": f"{RESULT_ENV_PREFIX}WEB_BUILD",
    "clippy": f"{RESULT_ENV_PREFIX}CLIPPY",
    "test": f"{RESULT_ENV_PREFIX}TEST",
    "audit": f"{RESULT_ENV_PREFIX}AUDIT",
}
UNCONDITIONAL_JOBS = {"changed", "repository-checks"}


def evaluate_results(environment: Mapping[str, str]) -> tuple[list[str], list[str]]:
    failures: list[str] = []
    errors: list[str] = []
    expected_env = set(EXPECTED_RESULT_ENV.values())
    provided_env = {
        name for name in environment if name.startswith(RESULT_ENV_PREFIX)
    }

    for name in sorted(expected_env - provided_env):
        errors.append(f"missing required job result environment variable: {name}")
    for name in sorted(provided_env - expected_env):
        errors.append(f"unexpected job result environment variable: {name}")

    for name, env_name in EXPECTED_RESULT_ENV.items():
        result = environment.get(env_name)
        if result is None:
            continue
        if result not in KNOWN_RESULTS:
            errors.append(f"unknown result for {name}: {result!r}")
        elif result != "success" and not (
            result == "skipped" and name not in UNCONDITIONAL_JOBS
        ):
            failures.append(f"{name}={result}")

    return failures, errors


def main(
    argv: Sequence[str] | None = None,
    environment: Mapping[str, str] | None = None,
) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments:
        print("job results must be supplied through the fixed CI environment", file=sys.stderr)
        return 2

    failures, errors = evaluate_results(
        os.environ if environment is None else environment
    )

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 2
    if failures:
        for failure in failures:
            print(f"required job did not pass: {failure}", file=sys.stderr)
        return 1

    print("All required CI jobs passed or were skipped")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
