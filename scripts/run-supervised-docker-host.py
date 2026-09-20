#!/usr/bin/env python3
"""Execute one pre-submitted declarative task on an isolated local Harness server.

This supervised client advertises only lease-proof support, not eval capabilities.
A trusted verifier runs in a separate offline container. See the companion guide.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
import re
import selectors
from pathlib import Path
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.request
import uuid

IMAGE_PREFIX = "sha256:"
LEASE_SECONDS = 300
OUTPUT_LIMIT = 8 * 1024 * 1024
GIT_HANDOFF_SCRIPT = Path(__file__).with_name("supervised_git_handoff.py").resolve()
TRUSTED_CONFIG = Path(__file__).resolve().parents[1] / "config" / "default.toml.example"
_QUALITY_GATE_SPEC = importlib.util.spec_from_file_location(
    "supervised_quality_gate", Path(__file__).with_name("supervised_quality_gate.py")
)
quality_gate = importlib.util.module_from_spec(_QUALITY_GATE_SPEC)
_QUALITY_GATE_SPEC.loader.exec_module(quality_gate)

HOST_OWNED_ARTIFACTS = {
    "runtime_host_usage",
    "supervised_input_snapshot",
    "supervised_candidate_resources",
    "supervised_docker_verification",
    "supervised_git_handoff",
}

def save(path: Path, value: dict) -> None:
    temporary = path.with_suffix(".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)

def docker(*args: str, timeout: int = 30) -> str:
    result = subprocess.run(
        ["docker", *args], capture_output=True, text=True, timeout=timeout
    )
    if result.returncode:
        raise RuntimeError(f"docker {args[0]} failed: {result.stderr.strip()}")
    return result.stdout.strip()

def stream_agent_output(command: list[str], root: Path, timeout: int, renew) -> int:
    """Keep a shared bounded prefix of both attached streams outside the candidate."""
    deadline = time.monotonic() + timeout
    observed = 0
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, bufsize=0)
    try:
        with selectors.DefaultSelector() as selector, \
                (root / "agent.jsonl").open("wb") as stdout, \
                (root / "agent.stderr").open("wb") as stderr:
            selector.register(process.stdout, selectors.EVENT_READ, stdout)
            selector.register(process.stderr, selectors.EVENT_READ, stderr)
            while selector.get_map() or process.poll() is None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f"agent exceeded {timeout}s wall deadline")
                renew()
                for key, _ in selector.select(min(0.2, remaining)):
                    chunk = os.read(key.fd, 64 * 1024)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    retained = chunk[:max(0, OUTPUT_LIMIT - observed)]
                    key.data.write(retained)
                    key.data.flush()
                    observed += len(chunk)
                    if observed > OUTPUT_LIMIT:
                        raise RuntimeError(f"agent output exceeded {OUTPUT_LIMIT} bytes")
            return process.wait(timeout=1)
    finally:
        # Disconnecting the CLI does not stop remote exec; the caller must remove
        # its exact container on failure before completing the job.
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
    blocks = re.findall(r"^```harness-activity-result\s*\n(.*?)^```[ \t]*$",
                        "\n".join(messages), re.MULTILINE | re.DOTALL)
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
    # The completion endpoint owns the full ActivityResult contract validation.
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

def archive_members(stream: tarfile.TarFile) -> list[tarfile.TarInfo]:
    members = stream.getmembers()
    if any(not (member.isfile() or member.isdir()) for member in members):
        raise RuntimeError("source archive contains a link or special file")
    return members

def extract_candidate(archive: Path, destination: Path) -> None:
    destination.mkdir(mode=0o700)
    with tarfile.open(archive) as stream:
        stream.extractall(destination, members=archive_members(stream), filter="data")

CANDIDATE_REAPER_SCRIPT = """import os, time
# PID 1 adopts agent descendants and must reap them before cgroup quiescence.
deadline = time.monotonic() + 900
while time.monotonic() < deadline:
    try:
        while time.monotonic() < deadline and os.waitpid(-1, os.WNOHANG)[0]:
            pass
    except ChildProcessError:
        pass  # No adopted children are waiting; keep the fixed retention deadline.
    time.sleep(0.01)
"""

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

class Host:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.root = args.state_dir.resolve()
        workspace = self.workspace = args.workspace.resolve()
        if workspace == self.root or workspace in self.root.parents or self.root in workspace.parents:
            raise RuntimeError("workspace and state directory must not overlap or contain one another")
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.lock = (self.root / "lock").open("w")
        fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        state = self.root / "state.json"
        self.state = json.loads(state.read_text()) if state.exists() else {
            "host_id": f"supervised-{uuid.uuid4().hex}", "phase": "new"
        }
        identity = {
            "server_url": args.server_url,
            "image": args.image, "verifier_image": args.verifier_image, "proxy_image": args.proxy_image,
            "model": args.model,
            "workspace": str(self.workspace), "base_commit": args.base_commit, "timeout": args.timeout,
            "synthetic_dns": args.synthetic_dns,
        }
        if "identity" in self.state and self.state["identity"] != identity:
            raise RuntimeError("resume arguments differ from the persisted run")
        self.state["identity"] = identity
        self.name = self.state["host_id"]
        self.endpoint = f"/api/runtime-hosts/{self.name}"
        self.last_renewal = 0.0
        self.token = os.environ["HARNESS_API_TOKEN"]

    def persist(self) -> None:
        save(self.root / "state.json", self.state)

    def api(self, path: str, body: dict) -> dict:
        request = urllib.request.Request(
            self.args.server_url.rstrip("/") + path,
            data=json.dumps(body).encode(),
            headers={"Authorization": f"Bearer {self.token}", "Content-Type": "application/json"},
        )
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            # Preserve server fencing/cancellation facts without recording credentials.
            detail = error.read().decode()
            raise RuntimeError(f"control plane returned {error.code}: {detail}") from error

    def renew(self) -> None:
        if time.monotonic() - self.last_renewal < 30:
            return
        self.api(self.endpoint + "/heartbeat", {})
        lease = self.state["lease"]
        response = self.api(
            self.endpoint + f"/runtime-jobs/{self.state['job']['id']}/lease/renew",
            {**lease, "renewal_id": str(uuid.uuid4()), "lease_secs": LEASE_SECONDS},
        )
        self.state["lease"] = {key: response[key] for key in lease}
        self.persist()
        self.last_renewal = time.monotonic()

    def base_args(self) -> list[str]:
        return [
            "--read-only", "--user", f"{os.getuid()}:{os.getgid()}",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--pids-limit", "128", "--memory", "2g", "--memory-swap", "2g", "--cpus", "1",
            "--tmpfs", f"/home/harness:rw,nosuid,nodev,size=128m,uid={os.getuid()},gid={os.getgid()},mode=700",
            "--tmpfs", "/tmp:rw,nosuid,nodev,size=64m",
        ]

    def cleanup(self) -> None:
        # Only exact names persisted by this invocation; never prune Docker globally.
        errors = []
        for name in [self.name + "-observer", self.name, self.name + "-verify", self.name + "-proxy"]:
            try:
                found = docker("ps", "-aq", "--filter", f"name=^/{name}$")
                if found:
                    docker("rm", "-f", name)
            except Exception as error:
                errors.append(f"{name}: {error}")
        try:
            networks = docker("network", "ls", "--format", "{{.Name}}").splitlines()
            if self.name in networks:
                docker("network", "rm", self.name)
        except Exception as error:
            errors.append(f"{self.name} network: {error}")
        if errors:
            self.state["cleanup_errors"] = errors
            self.persist()
            raise RuntimeError("cleanup incomplete: " + "; ".join(errors))
        self.state.pop("cleanup_errors", None)

    def freeze_input(self) -> None:
        result = subprocess.run(
            [sys.executable, "-I", str(GIT_HANDOFF_SCRIPT), "freeze", str(self.workspace),
             self.args.base_commit, str(self.root / "input.bundle")],
            capture_output=True, text=True, timeout=30,
        )
        if result.returncode:
            raise RuntimeError("input Git snapshot failed: " + result.stderr.strip())
        self.state["input_snapshot"] = json.loads(result.stdout)

    def launch(self) -> None:
        args = self.args
        snapshot = self.state["input_snapshot"]
        docker("network", "create", "--internal", self.name)
        proxy_args = [
            "run", "-d", "--name", self.name + "-proxy", "--network", "bridge",
            "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--pids-limit", "64", "--memory", "128m", "--cpus", "0.5",
            "--env", "HARNESS_EGRESS_ALLOWLIST=chatgpt.com,auth.openai.com",
        ]
        if args.synthetic_dns:
            proxy_args += ["--env", "HARNESS_EGRESS_ALLOW_RFC2544_DNS=1"]
        docker(*proxy_args, args.proxy_image)
        docker("network", "connect", "--alias", "proxy", self.name, self.name + "-proxy")
        run_args = [
            "run", "-d", "--name", self.name, "--network", self.name, *self.base_args(),
            "--tmpfs", f"/workspace:rw,nosuid,nodev,size=512m,uid={os.getuid()},gid={os.getgid()},mode=700",
            "--mount", f"type=bind,src={self.root / 'input.bundle'},dst=/input.bundle,readonly",
            "--mount", f"type=bind,src={GIT_HANDOFF_SCRIPT},dst=/git-handoff.py,readonly",
            "--mount", f"type=bind,src={args.auth_file.resolve()},dst=/run/codex-auth.json,readonly",
            "--env", "HTTPS_PROXY=http://proxy:8080", "--env", "HTTP_PROXY=http://proxy:8080",
            args.image, "python3", "-I", "-c", CANDIDATE_REAPER_SCRIPT,
        ]
        docker(*run_args)
        self.state["container_started"] = True
        self.persist()
        self.start_observer()
        docker("exec", self.name, "python3", "-I", "/git-handoff.py", "prepare",
               "/input.bundle", snapshot["base_commit"], snapshot["bundle_sha256"], "/workspace")
        docker("exec", self.name, "sh", "-c",
               'set -eu; mkdir -p /home/harness/.codex; '
               'cp /run/codex-auth.json /home/harness/.codex/auth.json')

    def start_observer(self) -> None:
        engine = json.loads(docker("info", "--format", "{{json .}}"))
        if engine["CgroupVersion"] != "2" or engine["CgroupDriver"] != "cgroupfs":
            raise RuntimeError("resource observer requires Docker cgroup v2 with cgroupfs")
        if os.getuid() == 0:
            raise RuntimeError("resource observer requires a non-root host UID")
        candidate_id = docker("inspect", self.name, "--format", "{{.Id}}")
        if len(candidate_id) != 64 or any(c not in "0123456789abcdef" for c in candidate_id):
            raise RuntimeError("Docker returned an invalid candidate container ID")
        self.state["candidate_id"] = candidate_id
        self.persist()
        docker("run", "-d", "--name", self.name + "-observer", "--network", "none",
               *self.base_args(), "--cgroupns", "host", "--mount",
               f"type=bind,src=/sys/fs/cgroup/docker/{candidate_id},dst=/sys/fs/cgroup,readonly",
               self.args.image, "sleep", "900")
        # Fail before model execution when this exact kernel/mount lacks the files.
        docker("exec", self.name + "-observer", "python3", "-I", "-c", CGROUP_METRICS_SCRIPT)

    def collect_resources(self, stop_agents: bool) -> None:
        if not self.state.get("container_started") or "resource_evidence" in self.state:
            return
        evidence = {"status": "incomplete", "candidate_id": self.state.get("candidate_id"),
                    "scope": "candidate cgroup through quiesced export; excludes verifier and proxy"}
        errors = []
        try:
            state = json.loads(docker("inspect", self.name, "--format", "{{json .State}}"))
            evidence["container_state"] = state
            if not state["Running"]:
                raise RuntimeError("candidate PID 1 exited before final resource collection")
            if stop_agents:
                docker("exec", self.name, "python3", "-I", "-c",
                       "import os,signal\ntry: os.kill(-1,signal.SIGKILL)\nexcept ProcessLookupError: pass")
        except Exception as error:
            errors.append(str(error))
        # The external reader can still retain kernel evidence when PID exhaustion
        # prevents candidate exec. Such a sample is explicitly not a final snapshot.
        try:
            metrics = json.loads(docker("exec", self.name + "-observer", "python3", "-I", "-c",
                                        CGROUP_METRICS_SCRIPT))
            evidence["metrics"] = metrics
            if metrics["current_pids_before"] != 1 or metrics["current_pids"] != 1:
                errors.append("candidate cgroup is not quiescent at final resource collection")
            if (metrics["memory_events"]["oom"] or metrics["memory_events"]["oom_kill"]
                    or metrics["pids_events"]["max"]):
                errors.append("candidate hit an OOM or PID limit")
        except Exception as error:
            errors.append(str(error))
        try:
            state = json.loads(docker("inspect", self.name, "--format", "{{json .State}}"))
            evidence["container_state"] = state
            if not state["Running"] or state["OOMKilled"]:
                errors.append("candidate PID 1 exited or Docker reported OOM")
        except Exception as error:
            errors.append(str(error))
        if errors:
            evidence["error"] = "; ".join(errors)
        else:
            evidence["status"] = "complete"
        self.state["resource_evidence"] = evidence
        save(self.root / "candidate-resources.json", evidence)
        self.persist()
        if evidence["status"] != "complete":
            raise RuntimeError("candidate resource evidence incomplete: " + evidence["error"])

    def wait(self) -> int:
        return stream_agent_output([
            "docker", "exec", "--workdir", "/workspace", self.name,
            "timeout", "--signal=KILL", str(self.args.timeout),
            "codex", "exec", "--skip-git-repo-check", "--json", "--sandbox", "danger-full-access",
            "-m", self.args.model, self.state["prepared_prompt"]["prompt"],
        ], self.root, self.args.timeout, self.renew)

    def capture(self) -> None:
        archive = self.root / "candidate.tar"
        with archive.open("wb") as output:
            # The idle PID 1 and this exporter survive kill(-1). All agent processes
            # share our unprivileged UID in this private PID namespace. Kill them
            # before exporting tmpfs so background children cannot race acceptance.
            subprocess.run(["docker", "exec", self.name, "python3", "-I", "-c",
                            "import os, signal, sys, time\nfrom pathlib import Path\n"
                            "try: os.kill(-1, signal.SIGKILL)\n"
                            "except ProcessLookupError: pass\n"
                            "deadline = time.monotonic() + 5\n"
                            "while int(Path('/sys/fs/cgroup/pids.current').read_text()) != 2:\n"
                            " if time.monotonic() >= deadline: raise RuntimeError('candidate did not quiesce before export')\n"
                            " time.sleep(0.01)\n"
                            "os.execvp('python3', ['python3', '-I', '/git-handoff.py', 'export', '/workspace', sys.argv[1]])",
                            self.state["input_snapshot"]["base_commit"]],
                           stdout=output, check=True, timeout=30)
        self.collect_resources(stop_agents=False)
        docker("rm", "-f", self.name)
        extract_candidate(archive, self.root / "candidate")
        revision = json.loads((self.root / "candidate/revision.json").read_text())
        if (revision.get("base_commit") != self.state["input_snapshot"]["base_commit"]
                or not isinstance(revision.get("candidate_commit"), str)
                or not re.fullmatch(r"[0-9a-f]{40}", revision["candidate_commit"])):
            raise RuntimeError("candidate Git revision does not match the pinned input")
        with (self.root / "candidate/candidate.bundle").open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        self.state["git_handoff"] = {**revision, "bundle_sha256": digest, "verified": False}
        self.persist()

    def retained_candidate(self) -> dict | None:
        handoff = self.state.get("git_handoff")
        bundle = self.root / "candidate" / "candidate.bundle"
        snapshot = self.root / "candidate" / "workspace"
        if not isinstance(handoff, dict) or not bundle.is_file() or not snapshot.is_dir():
            return None
        if not re.fullmatch(r"[0-9a-f]{40}", str(handoff.get("candidate_commit", ""))):
            return None
        if not re.fullmatch(r"[0-9a-f]{40}", str(handoff.get("base_commit", ""))):
            return None
        if not re.fullmatch(r"[0-9a-f]{64}", str(handoff.get("bundle_sha256", ""))):
            return None
        return handoff

    def hydrate_retained_candidate(self) -> dict | None:
        """Return a retained handoff, reconstructing pins from candidate/ when needed.

        A fresh --state-dir may receive only a copied candidate/ tree from a prior
        completed run. Do not require rewriting that prior run's phase or completion
        files: rebuild git_handoff from revision.json plus the on-disk bundle digest.
        """
        existing = self.retained_candidate()
        if existing is not None:
            return existing
        revision_path = self.root / "candidate" / "revision.json"
        bundle = self.root / "candidate" / "candidate.bundle"
        snapshot = self.root / "candidate" / "workspace"
        if not revision_path.is_file() or not bundle.is_file() or not snapshot.is_dir():
            return None
        revision = json.loads(revision_path.read_text())
        if not isinstance(revision, dict):
            return None
        if not re.fullmatch(r"[0-9a-f]{40}", str(revision.get("candidate_commit", ""))):
            return None
        if not re.fullmatch(r"[0-9a-f]{40}", str(revision.get("base_commit", ""))):
            return None
        if getattr(self, "args", None) is not None:
            if revision["base_commit"] != self.args.base_commit:
                raise RuntimeError("retained candidate base_commit does not match --base-commit")
        with bundle.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        self.state["git_handoff"] = {
            "base_commit": revision["base_commit"],
            "candidate_commit": revision["candidate_commit"],
            "bundle_sha256": digest,
            "verified": False,
        }
        self.persist()
        return self.retained_candidate()

    def prepare_follow_on_claim(self) -> None:
        # Keep the verified candidate; clear prior-job lease, result, and evidence.
        # Never delete a completed run's completion artifacts — use a fresh state dir.
        for name in ("completion.json", "completion-response.json"):
            if (self.root / name).exists():
                raise RuntimeError(
                    "refusing to mutate a completed run's "
                    + name
                    + "; copy candidate/ into a fresh --state-dir instead"
                )
        for key in ("job", "lease", "result", "agent_result", "prepared_prompt",
                    "cleanup_errors", "resource_evidence", "execution_evidence"):
            self.state.pop(key, None)
        for name in ("agent.jsonl", "agent.stderr", "verifier.stdout", "candidate-resources.json"):
            path = self.root / name
            if path.exists():
                path.unlink()
        self.state["phase"] = "claiming"
        self.persist()

    def offline_verify_args(self, *extra_mounts: str) -> list[str]:
        return [
            "run", "-d", "--name", self.name + "-verify", "--network", "none",
            *self.base_args(),
            "--tmpfs", f"/candidate:rw,nosuid,nodev,size=512m,uid={os.getuid()},gid={os.getgid()},mode=700",
            "--mount", f"type=bind,src={self.root / 'candidate'},dst=/handoff,readonly",
            "--mount", f"type=bind,src={GIT_HANDOFF_SCRIPT},dst=/git-handoff.py,readonly",
            "--mount", f"type=bind,src={self.root / 'verifier.py'},dst=/verify.py,readonly",
            "--mount", f"type=bind,src={TRUSTED_CONFIG},dst=/config.toml,readonly",
            *extra_mounts,
            "--entrypoint", "python3",
            self.args.verifier_image, "-I", "-c",
        ]

    def wait_verify_container(self) -> dict:
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            self.renew()
            state = json.loads(docker("inspect", self.name + "-verify", "--format", "{{json .State}}"))
            if not state["Running"]:
                output = docker("logs", self.name + "-verify")
                (self.root / "verifier.stdout").write_text(output)
                return state
            time.sleep(1)
        raise RuntimeError("independent verifier exceeded 120s deadline")

    def verify(self, candidate_commit: str | None = None) -> int:
        # Reconstruct verified Git blobs; override ENTRYPOINT (may be native Harness).
        handoff = self.state["git_handoff"]
        pin = candidate_commit or handoff["candidate_commit"]
        docker(*self.offline_verify_args(),
               "import subprocess,sys\n"
               "subprocess.run(['python3','-I','/git-handoff.py','verify',"
               "'/handoff/candidate.bundle',*sys.argv[1:],'/handoff/workspace','/candidate'],check=True)\n"
               "sys.exit(subprocess.call(['python3','-I','/verify.py','/candidate']))",
               handoff["base_commit"], pin, handoff["bundle_sha256"])
        return self.wait_verify_container()["ExitCode"]

    def verify_expected_head(self, expected_head_sha: str, validation_commands: list) -> list:
        """Bind retained-bundle reconstruction to the server-selected head, then run argv."""
        handoff = self.retained_candidate()
        if handoff is None:
            raise RuntimeError("native quality gate requires a retained candidate Git bundle")
        quality_gate.bind_retained_candidate(handoff, expected_head_sha)
        payload = quality_gate.validation_spec(handoff, expected_head_sha, validation_commands)
        out_dir = self.root / "quality-gate-out"
        if out_dir.exists():
            for path in out_dir.iterdir():
                path.unlink()
        else:
            out_dir.mkdir(mode=0o700)
        docker(*self.offline_verify_args(
            f"--mount=type=bind,src={self.root / 'verifier.py'},dst=/trusted/verify.py,readonly",
            f"--mount=type=bind,src={out_dir},dst=/out",
        ), quality_gate.OFFLINE_VALIDATION_SCRIPT, payload)
        self.wait_verify_container()
        return quality_gate.read_validation_evidence(
            out_dir / "validation.json", len(validation_commands)
        )

    def result(self, status: str, reason: str, artifacts: list | None = None,
               native: dict | None = None) -> dict:
        artifacts = list((native or {}).get("artifacts", [])) + list(artifacts or [])
        if "input_snapshot" in self.state:
            artifacts.append({"artifact_type": "supervised_input_snapshot",
                              "artifact": self.state["input_snapshot"]})
        if "git_handoff" in self.state:
            artifacts.append({"artifact_type": "supervised_git_handoff",
                              "artifact": self.state["git_handoff"]})
        if self.state.get("container_started"):
            evidence = self.state.get("resource_evidence", {"status": "incomplete", "error": "not collected"})
            artifacts.append({"artifact_type": "supervised_candidate_resources", "artifact": evidence})
            if status in {"succeeded", "succeeded_with_blockers"} and evidence["status"] != "complete":
                status, reason = "failed", reason + "; final resource evidence is incomplete"
        return {**(native or {}), "activity": self.state["job"]["input"]["activity"], "status": status,
                "summary": reason, "artifacts": artifacts or [],
                "signals": (native or {}).get("signals", []) if native and status == native["status"] else [],
                "error": (native or {}).get("error") if native and status == native["status"]
                else reason if status != "succeeded" else None}

    def complete(self) -> None:
        payload_path = self.root / "completion.json"
        if not payload_path.exists():
            payload = {**self.state["lease"], "result": self.state["result"]}
            evidence = self.state.get("execution_evidence")
            if evidence is not None:
                payload["execution_evidence"] = evidence
            save(payload_path, payload)
        response = self.api(self.endpoint + f"/runtime-jobs/{self.state['job']['id']}/complete",
                            json.loads(payload_path.read_text()))
        save(self.root / "completion-response.json", response)
        if response.get("completed") is not True:
            raise RuntimeError("completion was not accepted; evidence retained for reconciliation")
        self.state["phase"] = "completed"
        self.persist()

    def run_native_quality_gate(self) -> None:
        command = self.state["job"]["input"].get("command")
        if not isinstance(command, dict):
            command = {}
        expected = quality_gate.require_expected_head(command)
        commands = quality_gate.require_validation_commands(command)
        self.state["phase"] = "executing"
        self.persist()
        validation = self.verify_expected_head(expected, commands)
        self.state["git_handoff"]["verified"] = all(item["exit_code"] == 0 for item in validation)
        self.state["execution_evidence"] = quality_gate.execution_evidence(expected, validation)
        self.persist()
        native = quality_gate.activity_result(
            expected, self.state["git_handoff"]["candidate_commit"], validation
        )
        self.state["result"] = self.result(native["status"], native["summary"], [
            {"artifact_type": "supervised_docker_verification", "artifact": {
                "image": self.args.image,
                "verifier_image": self.args.verifier_image,
                "verifier_exit_code": 0 if native["status"] == "succeeded" else 1,
                "verifier_sha256": hashlib.sha256((self.root / "verifier.py").read_bytes()).hexdigest(),
                "candidate_sha256": hashlib.sha256(
                    (self.root / "candidate" / "candidate.bundle").read_bytes()
                ).hexdigest(),
                "expected_head_sha": expected,
                "full_eval_capabilities": False,
            }},
        ], native)

    def run(self) -> None:
        phase = self.state["phase"]
        if phase in {"preparing", "preparation_failed"}:
            raise RuntimeError("input preparation did not complete; retained state will not recopy or rerun: "
                               + self.state.get("preparation_error", "host interrupted"))
        if phase == "completed":
            print("already completed; no agent invocation")
            return
        if phase in {"executing", "claimed"}:
            reason = "host interrupted; candidate was not rerun"
            try:
                if self.state.get("container_started"):
                    self.collect_resources(stop_agents=True)
            except Exception as error:
                reason += "; " + str(error)
            self.state["result"] = self.result("failed", reason)
            self.state["phase"] = "cleaning"
            self.persist()
        if self.state["phase"] == "cleaning":
            self.cleanup()
            self.state["phase"] = "completing"
            self.persist()
        if self.state["phase"] == "completing":
            self.complete()
            return
        if phase == "new":
            self.state["request"] = json.loads(self.args.request.read_text())
            self.state["submission"] = json.loads(self.args.submission.read_text())
            (self.root / "verifier.py").write_bytes(self.args.verifier.read_bytes())
            if self.hydrate_retained_candidate():
                # Reuse a previously exported candidate; do not recopy operator source.
                self.prepare_follow_on_claim()
            else:
                self.state["phase"] = "preparing"
                self.persist()
                try:
                    self.freeze_input()
                except (Exception, KeyboardInterrupt) as error:
                    self.state["phase"] = "preparation_failed"
                    self.state["preparation_error"] = str(error) or "host interrupted by keyboard"
                    self.persist()
                    raise
                self.state["phase"] = "claiming"
                self.persist()
        self.api("/api/runtime-hosts/register", {
            "host_id": self.name, "capabilities": ["runtime_job_lease_proof_v1"]})
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            claim = self.api(self.endpoint + "/runtime-jobs/claim", {"lease_secs": LEASE_SECONDS, "execution_workspace": "/workspace"})
            if claim.get("claimed"):
                break
            time.sleep(1)
        else:
            raise RuntimeError("no compatible job claimed within 30 seconds")
        job = claim["runtime_job"]
        if job["input"]["workflow_id"] != self.state["submission"]["workflow_id"]:
            raise RuntimeError("claimed unrelated job; use a dedicated disposable server")
        command = job["input"].get("command")
        if not isinstance(command, dict):
            command = {}
        if "eval" in command or "agent_contract" in command or "exact_replay" in command:
            raise RuntimeError("this supervised client cannot execute eval or pinned-contract jobs")
        activity = job["input"].get("activity")
        if activity != quality_gate.QUALITY_GATE_ACTIVITY:
            request = self.state["request"]
            submission = self.state["submission"]
            digest = hashlib.sha256(b"\0".join(value.encode() for value in [
                str(Path(request["project"]).resolve()), request.get("subject_key") or request.get("external_id") or "",
                submission["task_id"], request["prompt"],
            ])).hexdigest()
            if command.get("prompt_ref") != "prompt-memory:" + digest:
                raise RuntimeError("request prompt does not match the claimed submission")
        self.state["job"] = job
        self.state["lease"] = {key: claim[key] for key in
                               ["lease_generation", "lease_expires_at", "lease_proof"]}
        self.state["phase"] = "claimed"
        self.persist()
        artifacts = []
        try:
            if activity == quality_gate.QUALITY_GATE_ACTIVITY:
                if claim.get("prepared_prompt") is not None:
                    raise RuntimeError("native quality gate must not include a prepared_prompt")
                self.run_native_quality_gate()
            else:
                prepared = claim.get("prepared_prompt")
                if not isinstance(prepared, dict) or not isinstance(prepared.get("prompt"), str) or not prepared["prompt"].strip():
                    raise RuntimeError("claim did not deliver a rendered activity prompt")
                if prepared.get("activity_result_schema", {}).get("activity") != job["input"]["activity"]:
                    raise RuntimeError("rendered result schema does not match the claimed activity")
                if not re.fullmatch(r"[0-9a-f]{64}", prepared.get("prompt_packet_digest", "")):
                    raise RuntimeError("claim did not deliver a valid prompt packet digest")
                self.state["prepared_prompt"] = prepared
                self.state["phase"] = "executing"
                self.persist()
                self.launch()
                exit_code = self.wait()
                if exit_code:
                    raise RuntimeError(f"agent exited with {exit_code}")
                usage = {"model": self.args.model, **read_usage(self.root / "agent.jsonl")}
                artifacts.append({"artifact_type": "runtime_host_usage", "artifact": usage})
                native = read_activity_result(self.root / "agent.jsonl", job["input"]["activity"])
                self.state["agent_result"] = native
                self.persist()
                for artifact in native.get("artifacts", []):
                    if artifact.get("artifact_type") in HOST_OWNED_ARTIFACTS:
                        raise RuntimeError("agent result contains host-owned artifact: " + artifact["artifact_type"])
                if native["status"] not in {"succeeded", "succeeded_with_blockers"}:
                    self.collect_resources(stop_agents=True)
                    self.state["result"] = self.result(native["status"], native["summary"], artifacts, native)
                else:
                    self.capture()
                    verifier_exit = self.verify()
                    if verifier_exit:
                        raise RuntimeError(f"independent verifier rejected candidate: exit {verifier_exit}")
                    self.state["git_handoff"]["verified"] = True
                    self.persist()
                    self.state["result"] = self.result(native["status"], native["summary"], artifacts + [
                        {"artifact_type": "supervised_docker_verification", "artifact": {
                            "image": self.args.image,
                            "verifier_image": self.args.verifier_image,
                            "verifier_exit_code": verifier_exit,
                            "verifier_sha256": hashlib.sha256((self.root / "verifier.py").read_bytes()).hexdigest(),
                            "candidate_sha256": hashlib.sha256((self.root / "candidate.tar").read_bytes()).hexdigest(),
                            "full_eval_capabilities": False,
                        }},
                    ], native)
        except (Exception, KeyboardInterrupt) as error:
            reason = "host interrupted by keyboard" if isinstance(error, KeyboardInterrupt) else str(error)
            try:
                if self.state.get("container_started"):
                    self.collect_resources(stop_agents=True)
            except Exception as collection_error:
                reason += "; " + str(collection_error)
            self.state["result"] = self.result("failed", reason, artifacts)
        finally:
            self.state["phase"] = "cleaning"
            self.persist()
            self.cleanup()
        self.state["phase"] = "completing"
        self.persist()
        self.complete()
        print(json.dumps(self.state["result"], indent=2))

def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server-url", required=True)
    for field in ["request", "submission", "workspace", "verifier", "auth-file", "state-dir"]:
        parser.add_argument("--" + field, required=True, type=Path)
    parser.add_argument("--base-commit", required=True, help="Full 40-character base commit SHA")
    parser.add_argument("--image", required=True, help="Pinned candidate agent image ID")
    parser.add_argument("--verifier-image", required=True, help="Pinned offline verifier image ID")
    parser.add_argument("--proxy-image", required=True)
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--synthetic-dns", action="store_true")
    args = parser.parse_args()
    if not all(value.startswith(IMAGE_PREFIX) and len(value) == 71
               for value in [args.image, args.verifier_image, args.proxy_image]):
        parser.error("images must be local sha256 image IDs")
    if args.verifier_image == args.image:
        parser.error("--verifier-image must differ from --image")
    if not 1 <= args.timeout <= 300:
        parser.error("timeout must be between 1 and 300 seconds")
    if not re.fullmatch(r"[0-9a-f]{40}", args.base_commit):
        parser.error("base-commit must be a full lowercase 40-character SHA")
    host = Host(args)
    host.run()
    if host.state.get("result", {}).get("status") != "succeeded":
        raise SystemExit(1)

if __name__ == "__main__":
    main()
