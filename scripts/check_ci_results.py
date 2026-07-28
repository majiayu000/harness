#!/usr/bin/env python3
"""Fail closed when any required GitHub Actions job did not succeed or skip."""

from __future__ import annotations

import sys
from collections.abc import Sequence


PASSING_RESULTS = {"success", "skipped"}
FAILING_RESULTS = {"failure", "cancelled"}
KNOWN_RESULTS = PASSING_RESULTS | FAILING_RESULTS


def evaluate_results(raw_results: Sequence[str]) -> tuple[list[str], list[str]]:
    failures: list[str] = []
    errors: list[str] = []
    seen: set[str] = set()

    if not raw_results:
        return failures, ["no required job results were provided"]

    for raw in raw_results:
        name, separator, result = raw.partition("=")
        if not separator or not name or not result:
            errors.append(f"invalid job result argument: {raw!r}")
            continue
        if name in seen:
            errors.append(f"duplicate job result: {name}")
            continue
        seen.add(name)

        if result not in KNOWN_RESULTS:
            errors.append(f"unknown result for {name}: {result!r}")
        elif result in FAILING_RESULTS:
            failures.append(f"{name}={result}")

    return failures, errors


def main(argv: Sequence[str] | None = None) -> int:
    raw_results = list(sys.argv[1:] if argv is None else argv)
    failures, errors = evaluate_results(raw_results)

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 2
    if failures:
        for failure in failures:
            print(f"required job failed or was cancelled: {failure}", file=sys.stderr)
        return 1

    print("All required CI jobs passed or were skipped")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
