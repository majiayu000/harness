#!/usr/bin/env python3
"""Summarise the agent-runtime trial log.

Usage: scripts/agent-runtime-trial-stats.py [log] [--since YYYY-MM-DD]
Default log: the backend's default path under the harness data directory.
Prints per agent: calls, success rate, outcomes, suspect false successes,
calls with leftovers, p50/p95 duration. Prompts and output are never logged.
"""
import json, os, platform, sys, datetime, collections


def default_log():
    if platform.system() == "Darwin":
        base = os.path.expanduser("~/Library/Application Support")
    else:
        base = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    return os.path.join(base, "harness", "agent-runtime-trial.jsonl")


args = [a for a in sys.argv[1:] if not a.startswith("--since")]
since = None
for a in sys.argv[1:]:
    if a.startswith("--since"):
        since = datetime.datetime.fromisoformat(a.split("=", 1)[1] if "=" in a else sys.argv[sys.argv.index(a) + 1])
path = args[0] if args and not args[0][0].isdigit() else default_log()
rows = []
for line in open(path):
    r = json.loads(line)
    if since and datetime.datetime.fromtimestamp(r["ts_ms"] / 1000) < since:
        continue
    rows.append(r)
by = collections.defaultdict(list)
for r in rows:
    by[(r["harness_agent"], r.get("channel"))].append(r)
print(f"{path}: {len(rows)} calls")
print(f"{'agent':10} {'channel':18} {'calls':>5} {'ok':>5} {'false_ok':>8} {'leftover':>8} {'p50_ms':>8} {'p95_ms':>8}  outcomes")
for (agent, channel), rs in sorted(by.items(), key=lambda kv: str(kv[0])):
    d = sorted(r["duration_ms"] for r in rs)
    pct = lambda p: d[min(len(d) - 1, int(p * len(d)))]
    outcomes = collections.Counter(r["outcome"] + (f"/{r['failure_class']}" if r.get("failure_class") else "") for r in rs)
    print(f"{agent:10} {str(channel):18} {len(rs):5} {sum(r['reported_success'] for r in rs):5} "
          f"{sum(r['suspect_false_success'] for r in rs):8} {sum(1 for r in rs if (r.get('leftovers') or 0) > 0):8} "
          f"{pct(0.5):8} {pct(0.95):8}  {dict(outcomes)}")
