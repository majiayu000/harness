#!/usr/bin/env python3
"""Line counts for docs/agent-runtime-backend.md. Run from the repository root."""
import glob, os

SRC = "crates/harness-agents/src"


def lines(path):
    text = open(path).read().split("\n")
    for i, line in enumerate(text):
        if line.startswith("#[cfg(test)]") and i + 1 < len(text) and text[i + 1].startswith("mod tests") and text[i + 1].rstrip().endswith("{"):
            text = text[:i]
            break
    return len(text), sum(1 for l in text if l.strip() and not l.strip().startswith("//"))


def total(paths):
    r = c = 0
    for p in paths:
        a, b = lines(p)
        r, c = r + a, c + b
    return r, c


files = sorted(glob.glob(f"{SRC}/**/*.rs", recursive=True))
nontest = [f for f in files if "test" not in os.path.relpath(f, SRC)]
before = {k: [f for f in nontest if os.path.relpath(f, SRC).startswith(k)] for k in ("claude", "codex", "opencode")}
for k, fs in before.items():
    print(f"before {k:9} files={len(fs):2} lines/code={total(fs)}")
print(f"before total            lines/code={total(sum(before.values(), []))}")
shared = [f"{SRC}/{n}" for n in ("spawn_supervisor.rs", "spawn_contract.rs", "process_cleanup.rs", "process_group.rs", "streaming.rs", "output_parsing.rs", "output_capture.rs", "provider_backpressure.rs", "lib.rs")]
shared += [f for f in nontest if os.path.relpath(f, SRC).startswith("spawn_contract/")]
print(f"shared (also Cursor)    lines/code={total([f for f in shared if os.path.exists(f)])}")
after = [f"{SRC}/agent_runtime_backend.rs", f"{SRC}/agent_runtime_backend/approval.rs", f"{SRC}/agent_runtime_backend/trial.rs"]
print(f"after backend           lines/code={total(after)}  (+ config struct, builder hook, AgentRegistry::replace: see git diff)")
