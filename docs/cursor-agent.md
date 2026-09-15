# Cursor Agent

Harness can execute Cursor Agent locally through `cursor-agent --print
--force --trust --output-format stream-json`. The `cursor` backend implements
`AgentBackend` and is available to the workflow runtime as `runtime_kind:
cursor`. It uses the existing managed-process launcher and cancellation cleanup.

## Configuration

Install Cursor Agent on the runtime host and run `cursor-agent login`, or supply
`CURSOR_API_KEY` in the server environment. Select a model available to that
account with `cursor-agent models`.

Update these sections in a complete Harness server configuration:

```toml
[agents]
default_agent = "cursor"
complexity_preferred_agents = ["cursor"]
capability_profile = "full"
sandbox_mode = "danger-full-access"

[agents.cursor]
cli_path = "cursor-agent"
default_model = "auto"

[isolation]
default_tier = "host"
network_allowlist = []
```

Use an absolute `cli_path` if the server's PATH does not include Cursor Agent.
The generic `agent` executable may refer to another installed application.
Authentication belongs to the operating-system user running the Harness server.
This configuration runs tasks with full tool access on that host.

For issue/PR workflows, select the runtime in the effective `WORKFLOW.md`:

```yaml
runtime_dispatch:
  runtime_kind: cursor
  timeout_secs: 3600
```

Remove Codex-specific `approval_policy` and `reasoning_effort` settings from
both the central and repository workflow files, including applicable activity
profiles. This repository currently sets `approval_policy: never` in both
`WORKFLOW.md` and `config/WORKFLOW.md`; a YAML null does not clear an inherited
value. Do this when switching the operational workflow to Cursor. Merely adding
`[agents.cursor]` does not switch existing Codex workflows.

A prompt submission can explicitly select Cursor:

```json
{
  "project": "/absolute/path/to/repository",
  "agent": "cursor",
  "prompt": "Read README.md and summarize the project without editing files.",
  "turn_timeout_secs": 300
}
```

Submit it to `POST /api/workflows/runtime/submissions`. Read its status and
artifacts through the submission endpoints. For issue/PR work, use the same
endpoint's structured `repo` plus `issue` or `pr` fields after configuring the
workflow. Existing independent-review and CI merge requirements still apply.

## Autonomous GitHub intake

Configure repository discovery instead of submitting each issue or PR manually:

```toml
[intake.github]
enabled = true
mode = "poll"
discovery_driver = "direct_rest"
poll_interval_secs = 300
planner_agent = "cursor"

[[intake.github.repos]]
repo = "owner/repository"
label = "" # Include all open issues and PRs, without a label filter.
project_root = "/absolute/path/to/repository"
```

GitHub REST polling discovers both issues and PRs. Uncovered issues enter
planning before implementation; PRs enter the native local-review gate.
Discovery uses no model session. Configure Cursor in each repository's runtime
workflow as described above so execution and independent local review use Cursor.
Existing workflows and linked PR coverage prevent duplicate issue work.

`pr_feedback.enabled: false` keeps PR intake on local review rather than remote
feedback sweeps. A passed local review makes the PR ready for merge approval
without requiring preconfigured server validation commands. Evaluator-owned
benchmark workflows retain their quality gate. Review findings still trigger repair and
an independent local review. Repository approval and CI requirements still apply.

## Execution visibility

One Harness turn launches a Cursor CLI session, which may perform many internal
model/tool steps. Intermediate assistant messages and completed tool results are
persisted separately from the final report. Tool-envelope metadata is not a tool
call. Cursor token usage and cost are unavailable; numeric defaults in legacy
transcripts are not observed consumption.

The runtime records agent activity only after the backend reports startup.
Review jobs that cannot acquire execution capacity return to the pending queue
without reserving a model turn. Issue and PR work takes priority over periodic
repository scans. A task in an implementation or review stage may still be queued.

## Supported boundaries

- CLI execution, message streaming, tool events, terminal output, and reported
  model identity are supported. Malformed output, missing success results,
  nonzero exits, and timeouts fail the attempt.
- Requests must explicitly use full permissions with no tool allowlist. Scoped
  requests (including deny-all structured-output correction turns) are rejected
  before spawning. Cursor's own configured deny rules still apply.
- Harness's spawn isolation and capability-token checks remain in effect.
  Container execution additionally needs a Cursor-equipped image and credentials;
  host login alone does not authenticate a container.
- This backend does not implement ACP, live steering, or interactive approvals.
  Workflow cancellation drops the execution and drains its managed process group.
- Cursor's documented CLI result has no enforceable USD cost or pinned JSON
  Schema contract. The backend claims neither capability and rejects direct
  USD budgets. Do not use it for pinned agent-contract jobs or enforced USD
  budgets. Normal workflow ActivityResult JSON is requested through prompts and
  validated by Harness; malformed output is not treated as successful work.

The official [headless guide](https://cursor.com/docs/cli/headless),
[output format](https://cursor.com/docs/cli/reference/output-format), and
[authentication guide](https://cursor.com/docs/cli/reference/authentication)
describe the upstream CLI contract. ACP is a separate future integration, not
required by this backend.
