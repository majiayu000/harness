#!/usr/bin/env python3
"""Execute one pre-submitted declarative task on an isolated local Harness server.

This client advertises lease proof, eval resource limits, and eval network policy.
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
    "supervised_verifier_resources",
    "supervised_docker_verification",
    "supervised_git_handoff",
    "resource_limit_report",
    "network_policy_report",
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

def stream_agent_output(command: list[str], root: Path, timeout: int, renew, **kwargs):
    kwargs.setdefault("output_limit", OUTPUT_LIMIT)
    return quality_gate.stream_agent_output(command, root, timeout, renew, **kwargs)

read_activity_result = quality_gate.read_activity_result
read_usage = quality_gate.read_usage

def archive_members(stream: tarfile.TarFile) -> list[tarfile.TarInfo]:
    members = stream.getmembers()
    if any(not (member.isfile() or member.isdir()) for member in members):
        raise RuntimeError("source archive contains a link or special file")
    return members

def extract_candidate(archive: Path, destination: Path) -> None:
    destination.mkdir(mode=0o700)
    with tarfile.open(archive) as stream:
        stream.extractall(destination, members=archive_members(stream), filter="data")

CANDIDATE_REAPER_SCRIPT = """import os, sys, time
# PID 1 adopts agent descendants and must reap them before cgroup quiescence.
deadline = time.monotonic() + float(sys.argv[1])
while time.monotonic() < deadline:
    try:
        while time.monotonic() < deadline and os.waitpid(-1, os.WNOHANG)[0]:
            pass
    except ChildProcessError:
        pass  # No adopted children are waiting; keep the retention deadline.
    time.sleep(0.01)
"""

CGROUP_METRICS_SCRIPT = quality_gate.CGROUP_METRICS_SCRIPT
DISK_METRICS_SCRIPT = quality_gate.DISK_METRICS_SCRIPT
disk_evidence_errors = quality_gate.disk_evidence_errors

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
        self.credential_variables = {}

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
            "--shm-size", "64m",
            "--tmpfs", f"/home/harness:rw,nosuid,nodev,size=128m,uid={os.getuid()},gid={os.getgid()},mode=700",
            "--tmpfs", "/tmp:rw,nosuid,nodev,size=64m",
        ]

    def isolation_args(self) -> list[str]:
        limits = self.state.get("enforced_limits")
        if not limits:
            return self.base_args()
        effective = limits["effective"]
        return quality_gate.docker_isolation_args(
            effective["disk_bytes"], os.getuid(), os.getgid(),
            memory_bytes=effective["memory_bytes"], pids=effective["pids"],
        )

    def cleanup(self) -> None:
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
        policy = self.state.get("enforced_network_policy")
        allowlist = "chatgpt.com,auth.openai.com" if policy is None else (
            None if policy["outbound"] == "deny" else ",".join(policy["network_allowlist"])
        )
        network = "none" if allowlist is None else self.name
        if allowlist is not None:
            docker("network", "create", "--internal", self.name)
            proxy_args = [
                "run", "-d", "--name", self.name + "-proxy", "--network", "bridge",
                "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                "--pids-limit", "64", "--memory", "128m", "--cpus", "0.5",
                "--env", f"HARNESS_EGRESS_ALLOWLIST={allowlist}",
            ]
            if args.synthetic_dns:
                proxy_args += ["--env", "HARNESS_EGRESS_ALLOW_RFC2544_DNS=1"]
            docker(*proxy_args, args.proxy_image)
            docker("network", "connect", "--alias", "proxy", self.name, self.name + "-proxy")
        limits = self.state.get("enforced_limits")
        retention_secs = max(
            900,
            (limits["effective"]["wall_time_secs"] if limits else args.timeout) + 300,
        )
        workspace = "512m" if limits is None else str(
            quality_gate.mount_budget(limits["effective"]["disk_bytes"])["workspace"]
        )
        run_args = [
            "run", "-d", "--name", self.name, "--network", network, *self.isolation_args(),
            "--tmpfs", f"/workspace:rw,nosuid,nodev,size={workspace},uid={os.getuid()},gid={os.getgid()},mode=700",
            "--mount", f"type=bind,src={self.root / 'input.bundle'},dst=/input.bundle,readonly",
            "--mount", f"type=bind,src={GIT_HANDOFF_SCRIPT},dst=/git-handoff.py,readonly",
        ]
        if limits is None:
            run_args += [
                "--mount", f"type=bind,src={args.auth_file.resolve()},dst=/run/codex-auth.json,readonly",
                "--env", "HTTPS_PROXY=http://proxy:8080", "--env", "HTTP_PROXY=http://proxy:8080",
            ]
        else:
            if allowlist is not None:
                run_args += ["--env", "HTTPS_PROXY=http://proxy:8080", "--env", "HTTP_PROXY=http://proxy:8080"]
        run_args += [args.image, "python3", "-I", "-c", CANDIDATE_REAPER_SCRIPT, str(retention_secs)]
        docker(*run_args)
        self.state["network_enforced"] = policy is not None
        self.state["container_started"] = True
        self.persist()
        self.start_observer(retention_secs)
        docker("exec", self.name, "python3", "-I", "/git-handoff.py", "prepare",
               "/input.bundle", snapshot["base_commit"], snapshot["bundle_sha256"], "/workspace")
        if limits is None:
            docker("exec", self.name, "sh", "-c",
                   'set -eu; mkdir -p /home/harness/.codex; '
                   'cp /run/codex-auth.json /home/harness/.codex/auth.json')

    def start_observer(self, retention_secs: int, target: str | None = None) -> None:
        engine = json.loads(docker("info", "--format", "{{json .}}"))
        if engine["CgroupVersion"] != "2" or engine["CgroupDriver"] != "cgroupfs":
            raise RuntimeError("resource observer requires Docker cgroup v2 with cgroupfs")
        if os.getuid() == 0:
            raise RuntimeError("resource observer requires a non-root host UID")
        candidate_id = docker("inspect", target or self.name, "--format", "{{.Id}}")
        if len(candidate_id) != 64 or any(c not in "0123456789abcdef" for c in candidate_id):
            raise RuntimeError("Docker returned an invalid candidate container ID")
        self.state["candidate_id"] = candidate_id
        self.persist()
        docker("run", "-d", "--name", self.name + "-observer", "--network", "none",
               *self.base_args(), "--cgroupns", "host", "--mount",
               f"type=bind,src=/sys/fs/cgroup/docker/{candidate_id},dst=/sys/fs/cgroup,readonly",
               self.args.image, "sleep", str(retention_secs))
        # Fail before model execution when this exact kernel/mount lacks the files.
        docker("exec", self.name + "-observer", "python3", "-I", "-c", CGROUP_METRICS_SCRIPT)

    def collect_resources(self, stop_agents: bool) -> None:
        if not self.state.get("container_started") or "resource_evidence" in self.state:
            return
        container = self.state.get("resource_container", self.name)
        verifier = container != self.name
        roots = ("/candidate", "/home/harness", "/tmp", "/dev/shm") if verifier else (
            "/workspace", "/home/harness", "/tmp", "/dev/shm")
        scope = ("offline verifier writable tmpfs roots under claimed eval limits" if verifier else
                 "candidate agent-writable tmpfs roots under trial policy")
        evidence = {"status": "incomplete", "candidate_id": self.state.get("candidate_id"),
                    "scope": ("offline verifier cgroup and writable tmpfs through quiescence" if verifier else
                              "candidate cgroup and agent-writable tmpfs through quiesced export; excludes verifier and proxy")}
        errors = []
        limit_exceeded = False
        try:
            state = json.loads(docker("inspect", container, "--format", "{{json .State}}"))
            evidence["container_state"] = state
            if not state["Running"]:
                raise RuntimeError("candidate PID 1 exited before final resource collection")
            if stop_agents:
                docker("exec", container, "python3", "-I", "-c",
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
                limit_exceeded = True
        except Exception as error:
            errors.append(str(error))
        # Disk accounting needs the live candidate mount namespace; measure after
        # quiescence checks so the sample is terminal, not a forged peak.
        try:
            disk = json.loads(docker("exec", container, "python3", "-I", "-c", DISK_METRICS_SCRIPT,
                                     ",".join(roots), scope))
            evidence["disk"] = disk
            errors.extend(disk_evidence_errors(disk, roots, scope))
        except Exception as error:
            errors.append(str(error))
        try:
            state = json.loads(docker("inspect", container, "--format", "{{json .State}}"))
            evidence["container_state"] = state
            if not state["Running"]:
                errors.append("candidate PID 1 exited before final resource collection")
            limit_exceeded = limit_exceeded or state["OOMKilled"]
        except Exception as error:
            errors.append(str(error))
        if errors:
            evidence["error"] = "; ".join(errors)
        elif limit_exceeded:
            evidence["status"] = "limit_exceeded"
            evidence["error"] = "candidate hit an OOM or PID limit"
        else:
            evidence["status"] = "complete"
        self.state["resource_evidence"] = evidence
        save(self.root / ("verifier-resources.json" if verifier else "candidate-resources.json"), evidence)
        self.persist()
        if evidence["status"] == "limit_exceeded":
            raise RuntimeError(evidence["error"])
        if evidence["status"] != "complete":
            raise RuntimeError("candidate resource evidence incomplete: " + evidence["error"])

    def wait(self) -> int:
        limits = (self.state.get("enforced_limits") or {}).get("effective")
        timeout = self.args.timeout if limits is None else limits["wall_time_secs"]
        account: dict = {}
        started = time.monotonic()
        last_poll = 0.0

        def check() -> None:
            nonlocal last_poll
            now = time.monotonic()
            if now - last_poll < 1:
                return
            last_poll = now
            metrics = json.loads(docker("exec", self.name + "-observer", "python3", "-I", "-c", CGROUP_METRICS_SCRIPT))
            if quality_gate.cpu_time_exceeded(metrics["cpu_time_micros"], limits["cpu_time_secs"]):
                raise RuntimeError("candidate exceeded cumulative CPU time limit")

        try:
            command = ["docker", "exec"]
            if limits is not None:
                command.append("-i")
            command += ["--workdir", "/workspace", self.name]
            if limits is not None:
                command += [
                    "python3", "-I", "-c",
                    "import json,os,sys\n"
                    "environment=os.environ.copy()\n"
                    "environment.update(json.load(sys.stdin))\n"
                    "os.execvpe(sys.argv[1],sys.argv[1:],environment)",
                ]
            command += [
                "timeout", "--signal=KILL", str(timeout), "codex", "exec",
                "--skip-git-repo-check", "--json", "--sandbox", "danger-full-access",
                "-m", self.args.model, self.state["prepared_prompt"]["prompt"],
            ]
            return stream_agent_output(command, self.root, timeout, self.renew,
               output_limit=None if limits is None else limits["output_bytes"],
               check=None if limits is None else check, account=None if limits is None else account,
               stdin_data=None if limits is None else json.dumps(self.credential_variables).encode())
        finally:
            if limits is not None:
                self.state["output_bytes"] = account.get("output_bytes", 0)
                self.state["wall_time_millis"] = max(0, int((time.monotonic() - started) * 1000))
                self.persist()

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
                    "cleanup_errors", "resource_evidence", "execution_evidence",
                    "enforced_limits", "enforced_network_policy", "network_enforced",
                    "resource_container", "output_bytes", "wall_time_millis"):
            self.state.pop(key, None)
        for name in ("agent.jsonl", "agent.stderr", "verifier.stdout", "candidate-resources.json",
                     "verifier-resources.json"):
            path = self.root / name
            if path.exists():
                path.unlink()
        self.state["phase"] = "claiming"
        self.persist()

    def offline_verify_args(self, *extra_mounts: str) -> list[str]:
        limits = self.state.get("enforced_limits")
        if limits is None:
            isolation = self.base_args()
            candidate_bytes = "512m"
        else:
            effective = limits["effective"]
            isolation = quality_gate.docker_isolation_args(
                effective["disk_bytes"], os.getuid(), os.getgid(),
                memory_bytes=effective["memory_bytes"], pids=effective["pids"],
            )
            candidate_bytes = str(quality_gate.mount_budget(effective["disk_bytes"])["workspace"])
        return [
            "run", "-d", "--name", self.name + "-verify", "--network", "none",
            *isolation,
            "--tmpfs", f"/candidate:rw,nosuid,nodev,size={candidate_bytes},uid={os.getuid()},gid={os.getgid()},mode=700",
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
        verify_args = self.offline_verify_args(
            f"--mount=type=bind,src={self.root / 'verifier.py'},dst=/trusted/verify.py,readonly",
            f"--mount=type=bind,src={out_dir},dst=/out",
        )
        limits = self.state.get("enforced_limits")
        if limits is None:
            docker(*verify_args, quality_gate.OFFLINE_VALIDATION_SCRIPT, payload)
            self.wait_verify_container()
        else:
            effective = limits["effective"]
            retention_secs = effective["wall_time_secs"] + 300
            docker(*verify_args[:-2], "-I", "-c", CANDIDATE_REAPER_SCRIPT, str(retention_secs))
            self.state["resource_container"] = self.name + "-verify"
            self.state["container_started"] = True
            self.state["network_enforced"] = True
            self.state["output_bytes"] = 0
            self.persist()
            self.start_observer(retention_secs, self.name + "-verify")
            spec = json.loads(payload)
            spec["output_limit"] = effective["output_bytes"]
            started = time.monotonic()
            try:
                stream_agent_output(
                    ["docker", "exec", "--workdir", "/candidate", self.name + "-verify",
                     "python3", "-I", "-c", quality_gate.OFFLINE_VALIDATION_SCRIPT, json.dumps(spec)],
                    self.root, effective["wall_time_secs"], self.renew,
                    output_limit=effective["output_bytes"],
                    check=lambda: self.check_verifier_cpu(effective["cpu_time_secs"]),
                )
            finally:
                self.state["output_bytes"] = sum(
                    (self.root / name).stat().st_size for name in ("agent.jsonl", "agent.stderr")
                    if (self.root / name).is_file()
                )
                self.state["wall_time_millis"] = max(0, int((time.monotonic() - started) * 1000))
                self.persist()
            validation_path = out_dir / "validation.json"
            if validation_path.is_file():
                self.state["output_bytes"] += sum(
                    item["output_bytes"] for item in json.loads(validation_path.read_text())
                )
                self.persist()
            self.collect_resources(stop_agents=True)
        return quality_gate.read_validation_evidence(
            out_dir / "validation.json", len(validation_commands)
        )

    def check_verifier_cpu(self, cpu_time_secs: int) -> None:
        metrics = json.loads(docker("exec", self.name + "-observer", "python3", "-I", "-c",
                                    CGROUP_METRICS_SCRIPT))
        if quality_gate.cpu_time_exceeded(metrics["cpu_time_micros"], cpu_time_secs):
            raise RuntimeError("verifier exceeded cumulative CPU time limit")

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
            artifact_type = ("supervised_verifier_resources" if self.state.get("resource_container") else
                             "supervised_candidate_resources")
            artifacts.append({"artifact_type": artifact_type, "artifact": evidence})
            if status in {"succeeded", "succeeded_with_blockers"} and evidence["status"] != "complete":
                status, reason = "failed", reason + "; " + evidence.get("error", "final resource evidence is incomplete")
        limits = self.state.get("enforced_limits")
        if limits is not None:
            try:
                report = quality_gate.report_from_evidence(
                    limits, self.state.get("resource_evidence") or {},
                    self.state["output_bytes"], self.state["wall_time_millis"],
                )
            except (RuntimeError, KeyError) as error:
                report = None
                if status in {"succeeded", "succeeded_with_blockers"}:
                    status, reason = "failed", f"{reason}; {error}"
            else:
                if report.get("termination") and status in {"succeeded", "succeeded_with_blockers"}:
                    status, reason = "failed", f"{reason}; {report['termination']['reason']}"
                artifacts.append({"artifact_type": "resource_limit_report", "artifact": report})
                usage = next((item["artifact"] for item in reversed(artifacts)
                              if item.get("artifact_type") == "runtime_host_usage"), None)
                checked = self.state.get("input_snapshot", {}).get("base_commit")
                if usage is not None and isinstance(checked, str) and "execution_evidence" not in self.state:
                    self.state["execution_evidence"] = quality_gate.execution_evidence(
                        checked, [], report, {
                            "model": usage["model"], "input_tokens": usage["input_tokens"],
                            "output_tokens": usage["output_tokens"],
                            "cached_input_tokens": usage.get("cached_input_tokens", 0),
                            "total_tokens": usage["total_tokens"], "cost_usd_micros": None,
                        })
        if self.state.get("network_enforced"):
            artifacts.append({"artifact_type": "network_policy_report", "artifact": quality_gate.network_report(
                self.state["job"]["id"], self.state["enforced_network_policy"],
                offline=bool(self.state.get("resource_container")))})
        return {**(native or {}), "activity": self.state["job"]["input"]["activity"], "status": status,
                "summary": reason, "artifacts": artifacts or [],
                "signals": (native or {}).get("signals", []) if native and status == native["status"] else [],
                "error": (native or {}).get("error") if native and status == native["status"]
                else reason if status != "succeeded" else None,
                "error_kind": (native or {}).get("error_kind") if native and status == native["status"]
                else "unknown" if status == "failed" else None}

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
        if self.state.get("enforced_limits") is None:
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
        if self.state.get("enforced_limits") is not None:
            report = next(
                artifact["artifact"] for artifact in self.state["result"]["artifacts"]
                if artifact["artifact_type"] == "resource_limit_report"
            )
            self.state["execution_evidence"] = quality_gate.execution_evidence(
                expected, validation, report
            )
            self.persist()

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
            "host_id": self.name, "capabilities": [
                "runtime_job_lease_proof_v1", "eval_resource_limits", "eval_network_policy",
                "trusted_eval_verifier_v1"]})
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
        self.state["job"] = job
        self.state["lease"] = {key: claim[key] for key in
                               ["lease_generation", "lease_expires_at", "lease_proof"]}
        self.state["phase"] = "claimed"
        self.persist()
        artifacts = []
        try:
            command = job["input"].get("command")
            if not isinstance(command, dict):
                command = {}
            if "agent_contract" in command or "exact_replay" in command:
                raise RuntimeError("this supervised client cannot execute pinned-contract jobs")
            activity = job["input"].get("activity")
            bound = quality_gate.bind_eval_contract(claim, job["input"])
            if bound is not None:
                eval_contract = command.get("eval") if isinstance(command.get("eval"), dict) else job["input"].get("eval", {})
                if eval_contract.get("base_commit") != self.args.base_commit:
                    raise RuntimeError("claimed eval base_commit does not match --base-commit")
                self.credential_variables = bound
                self.state["enforced_limits"] = claim["resource_limits"]
                self.state["enforced_network_policy"] = claim["network_policy"]
            if activity != quality_gate.QUALITY_GATE_ACTIVITY and bound is None:
                request = self.state["request"]
                submission = self.state["submission"]
                digest = hashlib.sha256(b"\0".join(value.encode() for value in [
                    str(Path(request["project"]).resolve()), request.get("subject_key") or request.get("external_id") or "",
                    submission["task_id"], request["prompt"],
                ])).hexdigest()
                if command.get("prompt_ref") != "prompt-memory:" + digest:
                    raise RuntimeError("request prompt does not match the claimed submission")
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
