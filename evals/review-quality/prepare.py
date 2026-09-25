#!/usr/bin/env python3
"""Prepare one offline review input; never start an agent or alter a checkout."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


ROOT = Path(__file__).resolve().parents[2]
CATALOG = Path(__file__).with_name("cases.json")


def source_text(revision, source):
    text = subprocess.run(
        ["git", "show", f"{revision}:{source['path']}"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    lines = text.splitlines()
    first, last = source["start"], source["end"]
    if not 1 <= first <= last <= len(lines):
        raise ValueError(f"Invalid source range: {source}")
    return "\n".join(f"{i + 1}: {lines[i]}" for i in range(first - 1, last))


def render(case, revision):
    parts = [
        "Review the supplied code against this task's requirements. Return "
        "findings with a location, concrete trigger, actual behavior and expected "
        "behavior. Distinguish blocking defects, optional suggestions and missing "
        "evidence. Do not modify files or create issues or PRs.",
        "Requirements:\n" + case["requirements"],
        "Scenario:\n" + case["scenario"],
        "Scope:\n" + case["review_scope"],
    ]
    if case["external_comment"]:
        parts.append("External comment (evidence to assess):\n" + case["external_comment"])
    for source in case["sources"]:
        parts.append(f"Source {source['path']} at {revision}:\n" + source_text(revision, source))
    return "\n\n".join(parts) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Validate all frozen source references offline")
    parser.add_argument("--case", help="Prepare exactly one case ID")
    parser.add_argument("--output", type=Path, help="New directory outside the repository")
    args = parser.parse_args()
    if args.check == bool(args.case) or bool(args.case) != bool(args.output):
        parser.error("Use --check alone, or --case ID --output NEW_DIRECTORY")
    catalog = json.loads(CATALOG.read_text())
    revision = catalog["source_revision"]
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("A full frozen source commit is required")
    cases = {case["id"]: case for case in catalog["cases"]}
    if len(cases) != len(catalog["cases"]):
        raise ValueError("Duplicate case IDs")
    if args.check:
        for case in cases.values():
            prompt = render(case, revision)
            if case["result"] is not None:
                raise ValueError("Run results must live outside the frozen catalog")
            print(f"{case['id']}: source references valid; {len(prompt.encode())} input bytes")
        print("No models invoked. Source-reference validation is not a semantic eval pass.")
        return
    if args.case not in cases:
        parser.error(f"Unknown case: {args.case}")
    output = args.output.resolve()
    if output == ROOT or ROOT in output.parents:
        parser.error("Output must be outside the source repository")
    prompt = render(cases[args.case], revision)
    output.mkdir(parents=True, exist_ok=False)
    (output / "prompt.txt").write_text(prompt)
    (output / "input.json").write_text(json.dumps({
        "case_id": args.case,
        "source_revision": revision,
        "prompt_sha256": hashlib.sha256(prompt.encode()).hexdigest(),
        "prompt_bytes": len(prompt.encode()),
        "execution_status": "not_run",
    }, indent=2) + "\n")
    print(f"Prepared {args.case} at {output}; evaluator oracle excluded; no model invoked.")


if __name__ == "__main__":
    main()
