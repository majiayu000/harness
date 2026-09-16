#!/usr/bin/env python3
"""Execute one pre-submitted declarative task on an isolated local Harness server.

This supervised client advertises only lease-proof support, not eval capabilities.
A trusted verifier runs in a separate offline container. See the companion guide.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
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


def extract_candidate(archive: Path, destination: Path) -> None:
    destination.mkdir()
    with tarfile.open(archive) as stream:
        # Reject links entirely: acceptance must not read outside the candidate.
        members = stream.getmembers()
        if any(not (member.isfile() or member.isdir()) for member in members):
            raise RuntimeError("candidate archive contains a link or special file")
        stream.extractall(destination, members=members, filter="data")


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
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.lock = (self.root / "lock").open("w")
        fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        state = self.root / "state.json"
        self.state = json.loads(state.read_text()) if state.exists() else {
            "host_id": f"supervised-{uuid.uuid4().hex}", "phase": "new"
        }
        identity = {
            "server_url": args.server_url,
            "image": args.image, "proxy_image": args.proxy_image, "model": args.model,
            "workspace": str(args.workspace.resolve()), "timeout": args.timeout,
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

    def launch(self) -> None:
        args = self.args
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
            "--mount", f"type=bind,src={args.workspace.resolve()},dst=/input,readonly",
            "--mount", f"type=bind,src={args.auth_file.resolve()},dst=/run/codex-auth.json,readonly",
            "--env", "HTTPS_PROXY=http://proxy:8080", "--env", "HTTP_PROXY=http://proxy:8080",
            args.image, "python3", "-I", "-c", CANDIDATE_REAPER_SCRIPT,
        ]
        docker(*run_args)
        self.state["container_started"] = True
        self.persist()
        self.start_observer()
        docker("exec", self.name, "sh", "-c",
               'set -eu; cp -R /input/. /workspace/; mkdir -p /home/harness/.codex; '
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
            "-m", self.args.model, self.state["request"]["prompt"],
        ], self.root, self.args.timeout, self.renew)

    def capture(self) -> None:
        archive = self.root / "candidate.tar"
        with archive.open("wb") as output:
            # The idle PID 1 and this exporter survive kill(-1). All agent processes
            # share our unprivileged UID in this private PID namespace. Kill them
            # before exporting tmpfs so background children cannot race acceptance.
            subprocess.run(["docker", "exec", self.name, "python3", "-I", "-c",
                            "import os, signal, time\nfrom pathlib import Path\n"
                            "try: os.kill(-1, signal.SIGKILL)\n"
                            "except ProcessLookupError: pass\n"
                            "deadline = time.monotonic() + 5\n"
                            "while int(Path('/sys/fs/cgroup/pids.current').read_text()) != 2:\n"
                            " if time.monotonic() >= deadline: raise RuntimeError('candidate did not quiesce before export')\n"
                            " time.sleep(0.01)\n"
                            "os.execvp('tar', ['tar', '-cf', '-', '-C', '/workspace', '.'])"],
                           stdout=output, check=True, timeout=30)
        self.collect_resources(stop_agents=False)
        docker("rm", "-f", self.name)
        extract_candidate(archive, self.root / "candidate")

    def verify(self) -> int:
        # No credentials, candidate execution, network, or writable host mount.
        docker("run", "-d", "--name", self.name + "-verify", "--network", "none",
               *self.base_args(),
               "--mount", f"type=bind,src={self.root / 'candidate'},dst=/candidate,readonly",
               "--mount", f"type=bind,src={self.root / 'verifier.py'},dst=/verify.py,readonly",
               self.args.image, "python3", "-I", "/verify.py", "/candidate")
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            self.renew()
            state = json.loads(docker("inspect", self.name + "-verify", "--format", "{{json .State}}"))
            if not state["Running"]:
                output = docker("logs", self.name + "-verify")
                (self.root / "verifier.stdout").write_text(output)
                return state["ExitCode"]
            time.sleep(1)
        raise RuntimeError("independent verifier exceeded 30s deadline")

    def result(self, status: str, reason: str, artifacts: list | None = None) -> dict:
        artifacts = list(artifacts or [])
        if self.state.get("container_started"):
            evidence = self.state.get("resource_evidence", {"status": "incomplete", "error": "not collected"})
            artifacts.append({"artifact_type": "supervised_candidate_resources", "artifact": evidence})
            if status == "succeeded" and evidence["status"] != "complete":
                status, reason = "failed", reason + "; final resource evidence is incomplete"
        return {"activity": self.state["job"]["input"]["activity"], "status": status,
                "summary": reason, "artifacts": artifacts or [],
                "error": reason if status != "succeeded" else None}

    def complete(self) -> None:
        payload_path = self.root / "completion.json"
        if not payload_path.exists():
            save(payload_path, {**self.state["lease"], "result": self.state["result"]})
        response = self.api(self.endpoint + f"/runtime-jobs/{self.state['job']['id']}/complete",
                            json.loads(payload_path.read_text()))
        save(self.root / "completion-response.json", response)
        if response.get("completed") is not True:
            raise RuntimeError("completion was not accepted; evidence retained for reconciliation")
        self.state["phase"] = "completed"
        self.persist()

    def run(self) -> None:
        phase = self.state["phase"]
        if phase == "completed":
            print("already completed; no agent invocation")
            return
        if phase in {"executing", "claimed"}:
            reason = "host interrupted; candidate was not rerun"
            try:
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
            self.state["phase"] = "claiming"
            self.persist()
        self.api("/api/runtime-hosts/register", {
            "host_id": self.name, "capabilities": ["runtime_job_lease_proof_v1"]})
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            claim = self.api(self.endpoint + "/runtime-jobs/claim", {"lease_secs": LEASE_SECONDS})
            if claim.get("claimed"):
                break
            time.sleep(1)
        else:
            raise RuntimeError("no compatible job claimed within 30 seconds")
        job = claim["runtime_job"]
        if job["input"]["workflow_id"] != self.state["submission"]["workflow_id"]:
            raise RuntimeError("claimed unrelated job; use a dedicated disposable server")
        request = self.state["request"]
        submission = self.state["submission"]
        digest = hashlib.sha256(b"\0".join(value.encode() for value in [
            str(Path(request["project"]).resolve()), request.get("subject_key") or request.get("external_id") or "",
            submission["task_id"], request["prompt"],
        ])).hexdigest()
        if job["input"]["command"].get("prompt_ref") != "prompt-memory:" + digest:
            raise RuntimeError("request prompt does not match the claimed submission")
        if "eval" in job["input"]["command"] or "agent_contract" in job["input"]["command"]:
            raise RuntimeError("this supervised client cannot execute eval or pinned-contract jobs")
        self.state["job"] = job
        self.state["lease"] = {key: claim[key] for key in
                               ["lease_generation", "lease_expires_at", "lease_proof"]}
        self.state["phase"] = "claimed"
        self.persist()
        artifacts = []
        try:
            self.state["phase"] = "executing"
            self.persist()
            self.launch()
            exit_code = self.wait()
            if exit_code:
                raise RuntimeError(f"agent exited with {exit_code}")
            usage = {"model": self.args.model, **read_usage(self.root / "agent.jsonl")}
            artifacts.append({"artifact_type": "runtime_host_usage", "artifact": usage})
            self.capture()
            verifier_exit = self.verify()
            if verifier_exit:
                raise RuntimeError(f"independent verifier rejected candidate: exit {verifier_exit}")
            self.state["result"] = self.result("succeeded", "Independent candidate verification passed.", artifacts + [
                {"artifact_type": "supervised_docker_verification", "artifact": {
                    "image": self.args.image, "verifier_exit_code": verifier_exit,
                    "verifier_sha256": hashlib.sha256((self.root / "verifier.py").read_bytes()).hexdigest(),
                    "candidate_sha256": hashlib.sha256((self.root / "candidate.tar").read_bytes()).hexdigest(),
                    "full_eval_capabilities": False,
                }},
            ])
        except (Exception, KeyboardInterrupt) as error:
            reason = "host interrupted by keyboard" if isinstance(error, KeyboardInterrupt) else str(error)
            try:
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
    parser.add_argument("--image", required=True)
    parser.add_argument("--proxy-image", required=True)
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--synthetic-dns", action="store_true")
    args = parser.parse_args()
    if not all(value.startswith(IMAGE_PREFIX) and len(value) == 71
               for value in [args.image, args.proxy_image]):
        parser.error("images must be local sha256 image IDs")
    if not 1 <= args.timeout <= 300:
        parser.error("timeout must be between 1 and 300 seconds")
    host = Host(args)
    host.run()
    if host.state.get("result", {}).get("status") != "succeeded":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
