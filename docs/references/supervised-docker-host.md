# Supervised local Docker runtime host

`scripts/run-supervised-docker-host.py` executes one already-submitted declarative
prompt task on a **dedicated disposable Harness server**. It is the first bounded
execution trial for GH-1768, not the complete nightly benchmark executor. It
advertises only `runtime_job_lease_proof_v1`; it must not advertise eval resource,
network-policy, or trusted-verifier capabilities or be used to create a baseline.

## Scope

The client claims one task, checks its workflow identity and persisted prompt
reference, runs Codex, verifies the exported candidate in an independent offline
container, removes its containers/network, and completes the leased job. The
server owns workflow state. No second scheduler, database access, or GitHub/git
subprocess is added to Harness crates.

Use a one-activity declarative workflow with `runtime_dispatch.runtime_kind:
remote_host`. Its activity policy must authorize the supplied request prompt;
the client requests server-rendered instructions for `/workspace` and retains the
prompt, packet digest and native result schema before model execution. Native
`run_quality_gate` claims reuse a retained candidate bundle instead of a model
prompt. It does not implement full multi-activity eval workflows, pinned agent
contracts, historical eval jobs, or PR lifecycle actions.
Submit through `POST /api/workflows/runtime/submissions` and retain both the exact
request JSON and response JSON. The server currently cannot filter host claims
by project: do not point this client at a shared server with unrelated jobs.

## Inputs and operation

Prepare a small Git repository with a full base commit SHA and an evaluator-owned
Python verifier. Prove that the verifier rejects the unchanged source before the
run. The verifier receives `/candidate` as its only argument and exits zero only
when the requested behavior is correct. It must not import acceptance criteria
from candidate-controlled tests. It runs in an offline container without model
credentials or access to the server token.

Use local Docker image IDs (`sha256:...`) for the candidate, verifier, and proxy
images. Build the existing `docker/agent/Dockerfile` and
`docker/egress-proxy/Dockerfile` if necessary. The agent image needs Codex, Git,
Python, and GNU `timeout`; it currently does not include the Rust toolchain. The
offline verifier image is independently pinned and must not be the candidate
image. Git trees containing symbolic links or submodules are rejected. This
client is intended for small repositories that fit its tmpfs limits.
The submitted task must explicitly require committing all changes and supply the
authorized Git author identity (for example through `git -c user.name=... -c
user.email=... commit`). The runner does not invent an author or append instructions
to the server-rendered prompt. Uncommitted or extra candidate files fail export.

Set `HARNESS_API_TOKEN` in the host environment, and run:

```sh
python3 scripts/run-supervised-docker-host.py \
  --server-url http://127.0.0.1:50211 \
  --request /private/run/request.json \
  --submission /private/run/submission.json \
  --workspace /private/run/source \
  --base-commit FULL_40_CHARACTER_BASE_SHA \
  --verifier /private/run/verify.py \
  --auth-file /private/run/codex-auth.json \
  --state-dir /private/run/host-state \
  --image sha256:AGENT_IMAGE_ID \
  --verifier-image sha256:VERIFIER_IMAGE_ID \
  --proxy-image sha256:PROXY_IMAGE_ID \
  --model gpt-5.5 --timeout 180
```

`--image` is the candidate agent image. `--verifier-image` is a separately pinned
offline verifier image ID (also `sha256:...`). Offline `verify` and
`verify_expected_head` run only in `--verifier-image`, never in the candidate
image. The verifier image may ship a native Harness `ENTRYPOINT`; the client
overrides it with an explicit `python3` entrypoint so reconstruction and
validation argv cannot be hijacked. A host-owned `config/default.toml.example`
is mounted read-only at `/config.toml` for native Harness validation commands.
Do not embed the trusted verifier into the candidate agent image.

Use a private temporary copy of the operator-authorized Codex `auth.json`, not a
mount of the entire host home directory. The client mounts it read-only and
copies it into the container's temporary home so token refresh cannot overwrite
the host login. Remove the temporary copy after the trial. The server token and
lease proof never enter either container. The agent can access its own model
credential, so this is a supervised trusted-task setup, not credential isolation
against a malicious agent.

`--synthetic-dns` enables the existing proxy option for RFC2544 synthetic DNS
addresses. Use it only when diagnostics establish that the local DNS/proxy uses
that range. The exact outbound allowlist remains `chatgpt.com,auth.openai.com`.
The agent connects only to an internal Docker network through the existing
first-party proxy; the verifier has `--network none`.

Both execution containers use a read-only root, non-root UID, dropped capabilities,
no-new-privileges, 128 PIDs, 2 GiB memory without swap, and a one-CPU rate limit.
The agent's writable workspace is a 512 MiB tmpfs, home 128 MiB, temporary directory
64 MiB, and `/dev/shm` 64 MiB (`--shm-size`; Docker's default shm is otherwise a large
uncapped agent-writable tmpfs). The host reads attached Codex stdout/stderr directly
into private host files, retaining at most 8 MiB across both streams. Exceeding that
shared limit fails the task and removes its container; transport buffers may contain
additional unretained bytes. Candidate files cannot replace the captured logs.
The host monitors wall time; GNU `timeout` separately bounds Codex if the host
process dies. These are explicit trial limits, **not** a
claim to implement every `CappedResourceLimits` field (especially aggregate CPU
time), nor a measured full eval resource report.

## Rendered activity results

The runner executes the persisted `prepared_prompt.prompt`, not the raw submission
text. Missing rendered instructions, digest or a mismatched schema activity fail
before model execution. The raw request remains an identity check for the claimed
submission. The packet digest is server-provided provenance, not a hash of the
rendered prompt text.

Only host-captured Codex `item.completed` agent messages supply the single fenced
`harness-activity-result` JSON object. Missing, duplicate, malformed or wrong-activity
results fail; tool output and candidate files cannot substitute for it. Native
artifacts, signals and validation records remain JSON values, and the completion
endpoint validates the full result contract. Agent artifacts cannot use the five
host-owned evidence types (`runtime_host_usage`, `supervised_input_snapshot`,
`supervised_candidate_resources`, `supervised_docker_verification`,
`supervised_git_handoff`); collisions
fail while retaining the original agent result for diagnosis. A successful agent result also needs
the existing independent offline verifier and resource evidence. A host failure
reports failure without forwarding success signals. Non-success agent outcomes
retain their status and evidence and do not become success merely because Codex
exited zero. Resume retains the delivered prompt and never invokes the model again.

## Pinned Git handoff

Before claiming a job, the runner exports `--base-commit` and only its reachable
history into `input.bundle`. The full lowercase SHA-1 and bundle SHA-256 are
persisted in `supervised_input_snapshot`. Later branches, worktree modifications,
and untracked files in the operator's source repository are not exported. The
canonical source path is resolved once for identity and preparation; source and
state directories cannot overlap. Existing bundle bytes are reused after a
restart while awaiting a claim. Failed or interrupted preparation cannot claim
or recopy from that state directory.

The agent mounts the bundle and trusted helper read-only. Before model execution,
the helper verifies the digest and exact base ref, imports into a fresh repository,
and materializes raw Git blobs into `/workspace` with a detached HEAD and index.
Git attributes, checkout filters, user configuration and hooks are not copied.
Only regular files and Git executable bits are supported. This includes the
base's reachable history: the operator must choose a base whose history is safe
to disclose. It is not a secret scanner or protection against an operator editing
private run state concurrently.

After stopping agent processes, the exporter reads candidate HEAD and copies only
Git object files into an isolated repository. It does not execute Git against the
candidate's configuration, hooks, index, alternates or replacement refs. The base
must be an ancestor of candidate HEAD. Every workspace file outside root `.git`
must match the commit's raw blobs and executable bits, including ignored files;
dirty, extra, linked or special files fail. Ordinary empty directories are ignored.
The host retains `candidate.tar`, containing `candidate.bundle`, `revision.json`
and the matching `workspace/` snapshot, then removes the candidate container.

The offline verifier receives that wrapper read-only at `/handoff`. The trusted
helper checks the pinned candidate bundle, ancestry and snapshot again, then
reconstructs raw commit files into a fresh `/candidate` tmpfs before invoking the
operator's verifier. The verifier never reads an agent-controlled working tree or
Git configuration. `supervised_git_handoff` records base/candidate commit IDs,
candidate bundle SHA-256 and whether independent verification succeeded. Failed
verification retains the pins with `verified: false` and drops success signals.

This is a single-task Git handoff plus an optional follow-on native quality-gate
claim that reuses the retained candidate bundle. A local candidate commit is not
an externally published PR head: quality-gate verification must match
`command.expected_head_sha`. Historical evaluation contracts and a formal
baseline remain unsupported; eval and pinned-contract jobs are rejected. There
is no source-directory-only mode or old-state migration.

## Native quality-gate consumption

After an implementation activity retains `candidate/candidate.bundle` and
`supervised_git_handoff`, claim a subsequent `run_quality_gate` job from a
**fresh** `--state-dir`. Copy only the prior run's `candidate/` tree into that
directory (or start with a state directory that already contains it). Do not
rewrite the completed run's `phase`, lease, result, or `completion*.json` files:
same-directory mutation of a completed run is refused. The fresh client rebuilds
`git_handoff` from `candidate/revision.json` plus the on-disk bundle digest,
skips source freeze, rejects `prepared_prompt` on the native claim, and requires
`command.expected_head_sha` plus `validation_commands_argv`. Point `--submission`
at the quality-gate workflow.

Verification reconstructs the retained bundle with the existing
`supervised_git_handoff.verify` helper inside `--verifier-image`, pinning the
candidate commit to `expected_head_sha`. A local candidate SHA that does not
equal the server-selected head fails closed; it is not treated as an externally
verified PR head. Offline validation runs the exact server argv against the
reconstructed tree (`/candidate`), with the operator verifier also mounted at
`/trusted/verify.py` for argv compatibility and `config/default.toml.example` at
`/config.toml`. Completion includes host `execution_evidence` whose
`checked_out_commit` is that expected head, with honest zero model usage and
without advertising `eval_resource_limits` or other eval capabilities. Prior-job
`execution_evidence` is cleared before the new claim so stale evidence cannot
attach to the current lease. Eval, agent-contract, and exact-replay jobs remain
rejected. Completed restarts of the original state directory still no-op and do
not reclaim.

## Candidate resource evidence

The candidate's trusted non-root Python PID 1 reaps adopted descendants until
its fixed 900-second retention deadline. A sleeping PID 1 left orphaned Codex
children as zombies, so strict cgroup quiescence could never complete even after
all live agent processes stopped. Reaping fixes that lifecycle defect without
relaxing the process-count checks. Completed model usage is parsed into the result
before export, so a later export/resource failure retains known usage; missing
usage is never synthesized.

The runner now retains candidate cgroup counters in the host-owned
`candidate-resources.json` and the `supervised_candidate_resources` result
artifact. It supports the observed Docker **cgroup v2 / cgroupfs** layout at
`/sys/fs/cgroup/docker/<full-container-id>` only. Missing paths, unsupported
engines, or a root operator UID fail before the model starts; there is no
alternative measurement fallback.

Immediately after candidate startup, an independent observer using the same
pinned image and non-root operator UID mounts **only that candidate's cgroup
subdirectory**, read-only, over its `/sys/fs/cgroup`. This replaces the default
cgroup mount even with `--cgroupns host`; sibling container cgroups are not
mounted. The observer has no network, credentials, Docker socket, added
capabilities, host PID namespace, or privileged mode. Its resource limits are
separate from the candidate's, so candidate PID exhaustion cannot block the
observer's reader.

Before removing the candidate, the runner stops agent processes and
waits up to five seconds for only the exporter and PID 1 to remain, then
exports and captures aggregate `cpu.stat` CPU microseconds, kernel `memory.peak`, `pids.peak`,
`pids.current`, `memory.events`, and `pids.events` through the observer. A complete
snapshot requires the candidate PID 1 still running and exactly one remaining
PID both before and after reading the other counters. Counters cover the candidate cgroup through export, including setup and
export overhead; they exclude the verifier, proxy and observer. CPU is recorded
in the kernel's microseconds, not inferred from a Docker CPU-rate setting.

After that cgroup sample, the host runs a terminal `statvfs` measurement inside the
still-running candidate against the four agent-writable tmpfs roots
(`/workspace`, `/home/harness`, `/tmp`, `/dev/shm`). Apparent file lengths
(`du -sb`) are not treated as allocated usage. Each mount records device identity,
fstype, used bytes, capacity, and fragment size; aggregates sum **distinct**
filesystems only. The sample is explicitly `sample_kind: terminal` with
`observed_at` — not a peak — because only an end-of-run observation is available.
Missing or malformed disk evidence fails closed like missing cgroup counters;
zeros are never synthesized. `/dev/mqueue` may appear writable for `access()` but
rejects ordinary file creates, so it is outside this disk accounting scope.

OOM or PID-limit events fail the task even if the agent or verifier would otherwise
claim success. Failure and interrupted-run recovery attempt collection before
cleanup. If quiescing or reading fails, the artifact explicitly records
`status: incomplete`, any available counters, and the collection error; the
original task failure is retained. Missing counters are never filled with zeros.
If candidate PID 1 dies, Docker may remove the cgroup files before collection;
that remains an incomplete failure, not a recovered final resource measurement.
Cleanup also removes the exact task-owned observer container.

This adds evidence to the supervised trial. It does **not** implement a complete
`ResourceLimitReport`, invent a `cpu_time_secs` lifetime quota, or map CPU-rate/wall
timeout into CPU-time usage, and it does not advertise `eval_resource_limits` or
enable formal eval jobs. Native quality-gate jobs that never start a candidate
container still omit `supervised_candidate_resources` rather than fabricating a
mapping.

A credential-free, no-model Docker probe using the pinned local
`harness-agent:fixture` image and test-sized tmpfs fixtures recorded argv, image
ID, `statvfs` samples, ENOSPC-at-cap checks, and cleanup in
[`supervised-tmpfs-disk-probe-2026-09-20.json`](./supervised-tmpfs-disk-probe-2026-09-20.json).
Observed used bytes are compared against filesystem capacities, not assumed equal
to payload lengths.

## Recovery and evidence

The private state directory contains the lease, task identity, snapshotted
verifier, candidate archive, bounded agent logs, completion payload, and server
response. Keep it outside the source tree and do not commit it. One file lock
prevents concurrent clients from resuming the same run.

Rerun with the same arguments/state directory after interruption. A completed
run does not invoke the model again. An interrupted execution is cleaned up and
reported as failed; it is never silently retried. A pending completion resends
its original persisted payload. If the server rejects an expired/stale lease,
the command fails and retains evidence for reconciliation; it never claims a new
job to conceal that failure. Captured log prefixes survive restart without being
replaced from candidate files; output not received before interruption is lost.
The idle task container expires 15 minutes after launch. Before candidate export,
the client kills residual agent processes inside that container's private PID
namespace, preserving its idle PID 1 and trusted exporter. It removes the task
container before independent verification. Stopping a tmpfs container loses its
candidate files, so interrupted runs are never resumed as successful candidates.

A failed task exits nonzero even when reporting its failure to Harness succeeds.
Missing usage stays missing; observed token counts are retained, and absent
pricing is not reported as zero spend. Full-suite baselines, self-hosted CI
runners and unattended scheduling remain separate work in GH-1768.

## Controlled trial: 2026-09-16

The candidate contained the actual preflight script from commit
`1646f104adbb9858de4f5c891f6c3ac43c5435fa`, before GH-2060, plus a one-activity
workflow configuration. The task was to reject hosts missing lease-proof or
network-policy capabilities. This was a source-file replay, not a full repository
checkout or one of the 15 historical benchmark cases.

The evaluator-owned verifier rejected the original file and accepted both
completed candidates. It exercised a fully capable host and each of the four
missing-capability cases. An independent file comparison confirmed that only
`scripts/preflight-eval-nightly.py` changed, adding two capability names.

| Attempt | Outcome | Evidence |
| --- | --- | --- |
| 1 | Failed during tmpfs artifact export; retained as failure | Submission `f88b17a5-8ab0-4178-becb-507a8e53ea32` |
| 2 | Passed independent offline verifier | Submission `df58491c-3ec3-48cc-8952-25addfe9ce73` |
| 3 | Injected 1-second timeout; failed and cleaned | Submission `30d828fe-2518-486b-8164-d522a2316bda` |
| 4 | Killed client after container start; resume failed without rerun | Submission `c427278e-b58a-4bc0-96ed-b174df5aebd4` |
| 5 | Passed final client path and independent verifier | Submission `ce936ef5-5cb4-4361-839f-b604d7e6e344` |

Attempt 2 recorded 57,590 total tokens, including 49,664 cached input tokens.
Attempt 5 recorded 121,151 total tokens, including 110,592 cached input tokens.
The separate login probe recorded 13,199 total tokens. These are observations
from completed Codex turns, not a complete spend accounting: the first failed
export did not retain token evidence, interrupted turns have no completed-turn
usage, and monetary cost is unknown. The generic submission detail endpoint did
not expose aggregate token usage for these remote jobs; observed usage is in the
completion artifacts and private logs.

The final client run exercised a lease renewal. A server restart preserved all
five complete submission responses byte-for-byte at the JSON-value level. The
database contained five runtime jobs (two succeeded, three failed) and 22 runtime
events, with no active job. Restarting an already-completed client did not invoke
the model. Simulated loss of the completion acknowledgement followed by replay
of the old lease returned `409 lease_lost`; the client retained its pending
completion and did not rerun. This is an explicit reconciliation boundary, not
a successful automatic recovery claim.

The proxy returned HTTP 403 for `example.com`, and direct external access from
the internal network failed. The initial model probe failed because local DNS
resolved `chatgpt.com` to `198.18.0.7`; the existing synthetic-DNS option allowed
the subsequent model request while preserving the exact-host allowlist.

Private raw evidence for this supervised session was retained at
`/Users/apple/harness-docker-eval-q966fzbk`. Model auth and server-token copies
are removed during teardown; they are not part of the report.


## Resource-evidence follow-up: 2026-09-17

A fresh disposable server/database used main `54a35ca3` plus the dispatcher fix
in PR #2069. The initial submission could not dispatch because the Linux control
plane's local sandbox rejected its network policy. PR #2069 separates that local
availability probe from remote execution while retaining the isolation contract,
budget and lease gates. The existing pending submission then dispatched normally;
no model ran during the initial claim timeout.

The same small preflight source-change task and independent verifier described
above were used. The unchanged source failed verification before execution.

| Attempt | Result | Evidence |
| --- | --- | --- |
| 1 | Model completed; export failed because sleeping PID 1 retained orphan zombies. Kept as failed, with incomplete resource evidence. | Submission `8aec5d24-4fcd-42c3-bccf-649f769d5d15` |
| 2 | Bounded reaper enabled; candidate export, independent verification and final resource snapshot succeeded. | Submission `c551a718-6a03-4767-a5b3-c30baed42c73` |

The successful candidate changed only `scripts/preflight-eval-nightly.py`.
Its cgroup snapshot recorded 1,087,475 CPU microseconds, 409,796,608 peak memory
bytes and 35 peak PIDs, with one remaining PID before and after reading counters.
No OOM or PID-limit event was recorded. These measurements include candidate
setup/export overhead and exclude the proxy, observer and verifier.

Attempt 1's retained completed-turn log records 76,168 tokens, including 66,432
cached input tokens; its old failure result did not attach that usage because
collection happened after export. The fix now retains known usage before export.
Attempt 2's completion artifact records 73,040 tokens, including 62,336 cached
input tokens. Actual USD cost remains unknown. Both attempts are retained; the
first was not rewritten as a success or silently retried.

Credential-free Docker probes also covered OOM, PID exhaustion, killed candidate
PID 1, and the old-vs-new orphan-reaping regression. Missing measurements remained
explicit failures. The model credential copy and task-owned containers/networks
were removed after each run. Both terminal submission responses were unchanged across a server restart, and
resuming the completed client left its model log unchanged without another model
invocation. The disposable server/database and their network were removed after
archiving; temporary server/database credential files were removed as well.
Raw operator evidence is retained at
`/Users/apple/harness-resource-trial-wc17fre7`.

This remains a source-only supervised trial. Formal historical multi-activity
execution, complete resource/network reporting and a full benchmark baseline
remain open in #1768.
