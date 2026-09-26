#!/usr/bin/env python3
"""Native quality-gate helpers for the supervised Docker host.

Binds a retained candidate Git bundle to command.expected_head_sha. Lease-only
claims keep an empty resource report. Eval claims carry the server's trusted
limits; this module does not turn a CPU rate or a wall timeout into CPU time.
"""
from __future__ import annotations

import json
import os
import re
import selectors
import subprocess
import sys
import time
from pathlib import Path

QUALITY_GATE_ACTIVITY = "run_quality_gate"
QUALITY_PASSED_SIGNAL = "QualityPassed"
QUALITY_FAILED_SIGNAL = "QualityFailed"

OFFLINE_VALIDATION_SCRIPT = """import hashlib,json,subprocess,sys,time
from pathlib import Path
spec=json.loads(sys.argv[1])
subprocess.run(['python3','-I','/git-handoff.py','verify',
'/handoff/candidate.bundle',spec['base'],spec['candidate'],spec['digest'],
'/handoff/workspace','/candidate'],check=True)
results=[]
for argv in spec['commands']:
  started=time.monotonic()
  proc=subprocess.run(argv,cwd='/candidate',capture_output=True)
  output=proc.stdout+proc.stderr
  results.append({'argv':argv,'exit_code':proc.returncode,
 'output_sha256':hashlib.sha256(output).hexdigest(),
 'duration_ms':int((time.monotonic()-started)*1000)})
Path('/out/validation.json').write_text(json.dumps(results))
sys.exit(0 if all(item['exit_code']==0 for item in results) else 1)
"""


def require_expected_head(command: dict) -> str:
    expected = command.get("expected_head_sha")
    if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{40}", expected):
        raise RuntimeError("native quality gate requires command.expected_head_sha")
    return expected


def require_validation_commands(command: dict) -> list[list[str]]:
    argv_list = command.get("validation_commands_argv")
    if not isinstance(argv_list, list) or not argv_list:
        raise RuntimeError("native quality gate requires validation_commands_argv")
    commands = []
    for argv in argv_list:
        if not isinstance(argv, list) or not argv or not all(
            isinstance(part, str) and part for part in argv
        ):
            raise RuntimeError("native quality gate validation argv is invalid")
        commands.append(list(argv))
    return commands


def bind_retained_candidate(handoff: dict, expected_head_sha: str) -> None:
    if handoff.get("candidate_commit") != expected_head_sha:
        raise RuntimeError(
            "retained candidate commit does not match command.expected_head_sha; "
            "local candidate SHA is not an externally verified PR head"
        )


def validation_spec(handoff: dict, expected_head_sha: str, commands: list[list[str]]) -> str:
    return json.dumps({
        "base": handoff["base_commit"],
        "candidate": expected_head_sha,
        "digest": handoff["bundle_sha256"],
        "commands": commands,
    })


def read_validation_evidence(path: Path, expected_count: int) -> list:
    if not path.is_file():
        raise RuntimeError("native quality gate validation evidence is missing")
    validation = json.loads(path.read_text())
    if not isinstance(validation, list) or len(validation) != expected_count:
        raise RuntimeError("native quality gate validation evidence is incomplete")
    return validation


_LIMIT_FIELDS = (
    "cpu_time_secs",
    "memory_bytes",
    "pids",
    "disk_bytes",
    "output_bytes",
    "wall_time_secs",
)
_SHM_BYTES = 64 * 1024 * 1024
_HOME_BYTES = 128 * 1024 * 1024
_TMP_BYTES = 64 * 1024 * 1024
_WORKSPACE_MIN_BYTES = 64 * 1024 * 1024
_VERIFIER_WORKSPACE_BYTES = 1024 * 1024


def _positive(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def reject_unsupported_eval(command: dict) -> None:
    required = command.get("eval", {}).get("required_runtime_host_capabilities", [])
    if not isinstance(required, list) or not all(isinstance(item, str) for item in required):
        raise RuntimeError("eval required capabilities are malformed")
    if "trusted_eval_verifier_v1" in required:
        raise RuntimeError("this supervised client does not implement trusted_eval_verifier_v1")


def trusted_resource_limits(claim: dict) -> dict:
    limits = claim.get("resource_limits")
    if (
        not isinstance(limits, dict)
        or not isinstance(limits.get("requested"), dict)
        or not isinstance(limits.get("effective"), dict)
    ):
        raise RuntimeError("eval claim is missing trusted resource_limits")
    effective = limits["effective"]
    for key in _LIMIT_FIELDS:
        if not _positive(effective.get(key)):
            raise RuntimeError(f"trusted resource limit {key} is missing")
    return limits


def cpu_time_exceeded(usage_usec: int, cpu_time_secs: int) -> bool:
    """Cumulative cgroup CPU time, not a --cpus rate or a wall-clock timeout."""
    if not isinstance(usage_usec, int) or isinstance(usage_usec, bool) or usage_usec < 0:
        raise RuntimeError("cpu usage is not a measured integer")
    if not _positive(cpu_time_secs):
        raise RuntimeError("trusted resource limit cpu_time_secs is missing")
    return usage_usec > cpu_time_secs * 1_000_000


def mount_budget(disk_bytes: int) -> dict[str, int]:
    if not _positive(disk_bytes):
        raise RuntimeError("trusted resource limit disk_bytes is missing")
    workspace = disk_bytes - _SHM_BYTES - _HOME_BYTES - _TMP_BYTES
    if workspace < _WORKSPACE_MIN_BYTES:
        raise RuntimeError("trusted disk limit is below the writable mount minimum")
    return {
        "workspace": workspace,
        "home": _HOME_BYTES,
        "tmp": _TMP_BYTES,
        "shm": _SHM_BYTES,
    }


def verifier_mounts(disk_bytes: int) -> dict[str, int]:
    budget = mount_budget(disk_bytes)
    candidate = budget["workspace"] - _VERIFIER_WORKSPACE_BYTES
    if candidate < _WORKSPACE_MIN_BYTES:
        raise RuntimeError("trusted disk limit is below the verifier mount minimum")
    return {**budget, "workspace": _VERIFIER_WORKSPACE_BYTES, "candidate": candidate}


def docker_isolation_args(
    disk_bytes: int, uid: int, gid: int, *, memory_bytes: int, pids: int
) -> list[str]:
    """Trusted memory, pid, and disk caps. A CPU rate is not the CPU-time budget."""
    budget = mount_budget(disk_bytes)
    return [
        "--read-only", "--user", f"{uid}:{gid}",
        "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
        "--pids-limit", str(pids),
        "--memory", str(memory_bytes), "--memory-swap", str(memory_bytes),
        "--shm-size", str(budget["shm"]),
        "--tmpfs", f"/home/harness:rw,nosuid,nodev,size={budget['home']},uid={uid},gid={gid},mode=700",
        "--tmpfs", f"/tmp:rw,nosuid,nodev,size={budget['tmp']}",
    ]


def network_policy_from_claim(claim: dict) -> dict:
    policy = claim.get("network_policy")
    if not isinstance(policy, dict):
        raise RuntimeError("eval claim is missing network_policy")
    allow = policy.get("network_allowlist", [])
    outbound = policy.get("outbound")
    if policy.get("inbound") != "deny" or outbound not in {"deny", "allowlist"}:
        raise RuntimeError("eval network policy is not enforceable by this container host")
    if (
        not isinstance(allow, list)
        or any(not isinstance(host, str) or not host or "," in host for host in allow)
    ):
        raise RuntimeError("eval network allowlist is invalid")
    if outbound == "deny" and allow:
        raise RuntimeError("deny outbound policy cannot include an allowlist")
    if outbound == "allowlist" and not allow:
        raise RuntimeError("allowlist outbound policy has no hosts")
    return policy


def network_report(job_id: str, policy: dict) -> dict:
    return {
        "runtime_job_id": job_id,
        "enforced": True,
        "policy": policy,
        "grants": [
            {"direction": "outbound", "host": host}
            for host in policy.get("network_allowlist", [])
        ],
        "connections": [],
        "payloads_recorded": False,
        "reason": "container egress follows the claimed eval network policy",
    }


def credential_variables(claim: dict) -> dict[str, str]:
    variables = claim.get("credential_environment_variables", {})
    if not isinstance(variables, dict):
        raise RuntimeError("eval credential environment is malformed")
    cleaned: dict[str, str] = {}
    for key, value in variables.items():
        if (
            not isinstance(key, str)
            or not key
            or any(char in key for char in "=\n\r\x00")
            or not isinstance(value, str)
        ):
            raise RuntimeError("eval credential environment variable is invalid")
        cleaned[key] = value
    return cleaned


def report_from_evidence(limits: dict, evidence: dict, output_bytes: int, wall_time_millis: int) -> dict:
    if not isinstance(evidence, dict) or evidence.get("status") not in {"complete", "limit_exceeded"}:
        raise RuntimeError("eval resource report requires complete measurements")
    metrics = evidence["metrics"]
    return build_resource_report(
        limits,
        cpu_usec=metrics["cpu_time_micros"],
        peak_memory_bytes=metrics["peak_memory_bytes"],
        peak_pids=metrics["peak_pids"],
        disk_bytes=evidence["disk"]["aggregate_used_bytes"],
        output_bytes=output_bytes,
        wall_time_millis=wall_time_millis,
        oom=bool(metrics["memory_events"]["oom"] or metrics["memory_events"]["oom_kill"]
                 or evidence.get("container_state", {}).get("OOMKilled")),
        pid_exhausted=bool(metrics["pids_events"]["max"]),
    )


def bind_eval_contract(claim: dict, job_input: dict) -> dict[str, str] | None:
    command = job_input.get("command")
    eval_contract = command.get("eval") if isinstance(command, dict) else None
    if not isinstance(eval_contract, dict):
        eval_contract = job_input.get("eval")
    if not isinstance(eval_contract, dict):
        return None
    reject_unsupported_eval({"eval": eval_contract})
    limits = trusted_resource_limits(claim)
    network_policy_from_claim(claim)
    mount_budget(limits["effective"]["disk_bytes"])
    return credential_variables(claim)


def build_resource_report(
    limits: dict,
    *,
    cpu_usec: int,
    peak_memory_bytes: int,
    peak_pids: int,
    disk_bytes: int,
    output_bytes: int,
    wall_time_millis: int,
    oom: bool = False,
    pid_exhausted: bool = False,
) -> dict:
    measurements = {
        "cpu_usec": cpu_usec,
        "peak_memory_bytes": peak_memory_bytes,
        "peak_pids": peak_pids,
        "disk_bytes": disk_bytes,
        "output_bytes": output_bytes,
        "wall_time_millis": wall_time_millis,
    }
    for name, value in measurements.items():
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise RuntimeError(f"resource measurement {name} is missing")
    effective = limits["effective"]
    usage = {
        "cpu_time_millis": cpu_usec // 1000,
        "peak_memory_bytes": peak_memory_bytes,
        "peak_pids": peak_pids,
        "disk_bytes": disk_bytes,
        "output_bytes": output_bytes,
        "wall_time_millis": wall_time_millis,
    }
    termination = None
    if cpu_time_exceeded(cpu_usec, effective["cpu_time_secs"]):
        termination = {
            "resource": "cpu_time",
            "reason": "cumulative CPU time limit exceeded",
        }
    elif oom or peak_memory_bytes > effective["memory_bytes"]:
        termination = {"resource": "memory", "reason": "memory limit exceeded"}
    elif pid_exhausted or peak_pids > effective["pids"]:
        termination = {"resource": "pids", "reason": "process limit exceeded"}
    elif disk_bytes > effective["disk_bytes"]:
        termination = {"resource": "disk", "reason": "disk limit exceeded"}
    elif output_bytes > effective["output_bytes"]:
        termination = {"resource": "output", "reason": "output limit exceeded"}
    elif wall_time_millis > effective["wall_time_secs"] * 1000:
        termination = {"resource": "wall_time", "reason": "wall-clock limit exceeded"}
    report = {
        "limits": limits,
        "usage": usage,
        "reason": termination["reason"] if termination else "completed within trusted resource limits",
    }
    if termination is not None:
        report["termination"] = termination
    return report


def execution_evidence(
    checked_out_commit: str,
    validation: list,
    resource_limit_report: dict | None = None,
    usage: dict | None = None,
) -> dict:
    # Lease-only client: do not invent eval resource enforcement measurements.
    if resource_limit_report is None:
        resource_limit_report = {
            "limits": {"requested": {}, "effective": {}, "caps": []},
            "usage": {},
            "reason": "lease-only supervised client; eval_resource_limits not advertised",
        }
    if usage is None:
        usage = {
            "model": "",
            "input_tokens": 0,
            "output_tokens": 0,
            "cached_input_tokens": 0,
            "total_tokens": 0,
            "cost_usd_micros": None,
        }
    return {
        "checked_out_commit": checked_out_commit,
        "resource_limit_report": resource_limit_report,
        "usage": usage,
        "isolation_cleanup_status": "cleaned",
        "validation": validation,
    }


def activity_result(expected_head_sha: str, candidate_commit: str, validation: list) -> dict:
    passed = all(item["exit_code"] == 0 for item in validation)
    status = "succeeded" if passed else "failed"
    signal = QUALITY_PASSED_SIGNAL if passed else QUALITY_FAILED_SIGNAL
    summary = (
        "Native quality gate matched expected_head_sha and validation passed."
        if passed
        else "Native quality gate validation failed against expected_head_sha."
    )
    return {
        "activity": QUALITY_GATE_ACTIVITY,
        "status": status,
        "summary": summary,
        "artifacts": [],
        "signals": (
            [{
                "signal_type": signal,
                "signal": {
                    "expected_head_sha": expected_head_sha,
                    "candidate_commit": candidate_commit,
                },
            }]
            if passed
            else []
        ),
        "validation": [
            {
                "command": " ".join(item["argv"]),
                "status": "passed" if item["exit_code"] == 0 else "failed",
            }
            for item in validation
        ],
        "error": None if passed else "revision-bound validation command failed",
        "error_kind": None if passed else "fatal",
    }


OUTPUT_LIMIT = 8 * 1024 * 1024

CGROUP_METRICS_SCRIPT = """import json
from pathlib import Path
root = Path('/sys/fs/cgroup')
def counters(name):
    return {key: int(value) for key, value in
            (line.split() for line in (root / name).read_text().splitlines())}
pids_before = int((root / 'pids.current').read_text())
cpu = counters('cpu.stat')
print(json.dumps({'cpu_time_micros': cpu['usage_usec'], 'current_pids_before': pids_before,
                  'peak_memory_bytes': int((root / 'memory.peak').read_text()),
                  'peak_pids': int((root / 'pids.peak').read_text()),
                  'memory_events': counters('memory.events'),
                  'pids_events': counters('pids.events'),
                  'current_pids': int((root / 'pids.current').read_text())}))
"""

DISK_METRICS_SCRIPT = """import json, os
from datetime import datetime, timezone
from pathlib import Path
ROOTS = ('/workspace', '/home/harness', '/tmp', '/dev/shm')
mounts = {}
for line in Path('/proc/self/mountinfo').read_text().splitlines():
    parts = line.split()
    sep = parts.index('-')
    mounts[parts[4]] = {'device': parts[2], 'fstype': parts[sep + 1]}
samples = []
for root in ROOTS:
    if not Path(root).exists():
        raise SystemExit('missing mount root: ' + root)
    if not os.access(root, os.W_OK):
        raise SystemExit('mount root not writable: ' + root)
    info = mounts.get(root)
    if info is None:
        raise SystemExit('mountinfo missing root: ' + root)
    st = os.statvfs(root)
    if st.f_frsize <= 0 or st.f_blocks <= 0 or st.f_bfree < 0 or st.f_bfree > st.f_blocks:
        raise SystemExit('malformed statvfs for ' + root)
    used = (st.f_blocks - st.f_bfree) * st.f_frsize
    capacity = st.f_blocks * st.f_frsize
    samples.append({'root': root, 'device': info['device'], 'fstype': info['fstype'],
                    'used_bytes': used, 'capacity_bytes': capacity, 'frsize': st.f_frsize})
by_device = {}
for sample in samples:
    prior = by_device.get(sample['device'])
    if prior is None:
        by_device[sample['device']] = sample
    elif (prior['used_bytes'] != sample['used_bytes']
          or prior['capacity_bytes'] != sample['capacity_bytes']):
        raise SystemExit('inconsistent statvfs for device ' + sample['device'])
print(json.dumps({
    'sample_kind': 'terminal',
    'observed_at': datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ'),
    'scope': 'candidate agent-writable tmpfs roots under trial policy',
    'mounts': samples,
    'aggregate_used_bytes': sum(item['used_bytes'] for item in by_device.values()),
    'aggregate_capacity_bytes': sum(item['capacity_bytes'] for item in by_device.values()),
    'distinct_filesystems': len(by_device),
}))
"""


def disk_evidence_errors(disk: object) -> list[str]:
    """Return problems that must block successful completion; never zero-fill."""
    if not isinstance(disk, dict):
        return ["disk evidence is not an object"]
    errors = []
    if disk.get("sample_kind") != "terminal":
        errors.append("disk sample_kind must be terminal (not peak)")
    if not isinstance(disk.get("observed_at"), str) or not disk["observed_at"]:
        errors.append("disk observed_at is missing")
    if disk.get("scope") != "candidate agent-writable tmpfs roots under trial policy":
        errors.append("disk scope is missing or unexpected")
    mounts = disk.get("mounts")
    expected = ("/workspace", "/home/harness", "/tmp", "/dev/shm")
    if not isinstance(mounts, list) or [item.get("root") for item in mounts] != list(expected):
        errors.append("disk mounts must cover the four agent-writable tmpfs roots")
        return errors
    by_device: dict = {}
    for item, root in zip(mounts, expected):
        if not isinstance(item, dict):
            errors.append(f"disk mount {root} is malformed")
            continue
        for key in ("device", "fstype"):
            if not isinstance(item.get(key), str) or not item[key]:
                errors.append(f"disk mount {root} missing {key}")
        for key in ("used_bytes", "capacity_bytes", "frsize"):
            value = item.get(key)
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                errors.append(f"disk mount {root} {key} must be a non-negative int")
        used, capacity = item.get("used_bytes"), item.get("capacity_bytes")
        if isinstance(used, int) and isinstance(capacity, int) and used > capacity:
            errors.append(f"disk mount {root} used_bytes exceeds capacity")
        device = item.get("device")
        if isinstance(device, str) and device not in by_device:
            if isinstance(used, int) and isinstance(capacity, int):
                by_device[device] = (used, capacity)
    if not isinstance(disk.get("distinct_filesystems"), int) or disk["distinct_filesystems"] != len(by_device):
        errors.append("disk distinct_filesystems does not match mount devices")
    expected_used = sum(pair[0] for pair in by_device.values())
    expected_capacity = sum(pair[1] for pair in by_device.values())
    if disk.get("aggregate_used_bytes") != expected_used:
        errors.append("disk aggregate_used_bytes does not match distinct filesystems")
    if disk.get("aggregate_capacity_bytes") != expected_capacity:
        errors.append("disk aggregate_capacity_bytes does not match distinct filesystems")
    return errors


def stream_agent_output(
    command: list[str], root: Path, timeout: int, renew, *,
    output_limit: int | None = None, check=None, account: dict | None = None,
    stdin_data: bytes | None = None,
) -> int:
    """Keep a shared bounded prefix of both attached streams outside the candidate."""
    if output_limit is None:
        output_limit = OUTPUT_LIMIT
    deadline = time.monotonic() + timeout
    observed = 0
    process = subprocess.Popen(
        command, stdin=subprocess.PIPE if stdin_data is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
    )
    try:
        if stdin_data is not None:
            process.stdin.write(stdin_data)
            process.stdin.close()
        with selectors.DefaultSelector() as selector, \
                (root / "agent.jsonl").open("wb") as stdout, \
                (root / "agent.stderr").open("wb") as stderr:
            selector.register(process.stdout, selectors.EVENT_READ, stdout)
            selector.register(process.stderr, selectors.EVENT_READ, stderr)
            while selector.get_map() or process.poll() is None:
                if account is not None:
                    account["output_bytes"] = observed
                if check is not None:
                    check()
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f"agent exceeded {timeout}s wall deadline")
                renew()
                for key, _ in selector.select(min(0.2, remaining)):
                    chunk = os.read(key.fd, 64 * 1024)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    retained = chunk[:max(0, output_limit - observed)]
                    key.data.write(retained)
                    key.data.flush()
                    observed += len(chunk)
                    if account is not None:
                        account["output_bytes"] = observed
                    if observed > output_limit:
                        raise RuntimeError(f"agent output exceeded {output_limit} bytes")
            if account is not None:
                account["output_bytes"] = observed
            return process.wait(timeout=1)
    finally:
        primary_error = sys.exc_info()[1]
        cleanup_errors = []
        try:
            if process.poll() is None:
                process.kill()
        except Exception as error:
            cleanup_errors.append(f"kill: {error}")
        try:
            process.wait(timeout=5)
        except Exception as error:
            cleanup_errors.append(f"wait: {error}")
        for stream in (process.stdout, process.stderr):
            try:
                stream.close()
            except Exception as error:
                cleanup_errors.append(f"close stream: {error}")
        if cleanup_errors:
            reason = "agent output cleanup failed: " + "; ".join(cleanup_errors)
            if primary_error is not None:
                reason = f"{type(primary_error).__name__}: {primary_error}; {reason}"
            raise RuntimeError(reason) from primary_error


def read_activity_result(log: Path, activity: str) -> dict:
    # Only host-captured assistant messages carry results; tool output is untrusted.
    messages = []
    for line in log.read_text(encoding="utf-8").splitlines():
        event = json.loads(line)
        if event.get("type") == "item.completed" and event.get("item", {}).get("type") == "agent_message":
            messages.append(event["item"]["text"])
    blocks = re.findall(
        r"^```harness-activity-result\s*\n(.*?)^```[ \t]*$",
        "\n".join(messages),
        re.MULTILINE | re.DOTALL,
    )
    if len(blocks) != 1:
        raise RuntimeError("agent must emit exactly one harness-activity-result block")
    result = json.loads(blocks[0])
    if not isinstance(result, dict) or result.get("activity") != activity:
        raise RuntimeError("agent result does not match the claimed activity")
    if not isinstance(result.get("status"), str) or not isinstance(result.get("summary"), str):
        raise RuntimeError("agent result requires status and summary strings")
    for field in ("artifacts", "signals", "validation"):
        if not isinstance(result.get(field, []), list):
            raise RuntimeError(f"agent result {field} must be an array")
    return result


def read_usage(log: Path) -> dict:
    for line in reversed(log.read_text(encoding="utf-8").splitlines()):
        event = json.loads(line)
        if event.get("type") == "turn.completed":
            usage = event["usage"]
            return {
                "input_tokens": usage["input_tokens"],
                "output_tokens": usage["output_tokens"],
                "cached_input_tokens": usage.get("cached_input_tokens", 0),
                "total_tokens": usage["input_tokens"] + usage["output_tokens"],
            }
    raise RuntimeError("agent produced no completed-turn usage evidence")
