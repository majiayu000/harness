# Local-First Review Provider Spec

Status: Draft
Date: 2026-05-11

## Goal

Make Harness review gates work reliably without depending on GitHub-hosted review bots.

The default review path should be local-first:

- run a required local `codex review` pass on the developer machine or runtime host
- optionally run Harness' existing agent-review loop
- treat GitHub-hosted Gemini and Codex review bots as advisory unless explicitly required
- preserve one normalized review result model regardless of provider

This spec defines the provider model, configuration shape, execution flow, output contract,
migration path, and verification plan.

## Problem

Harness currently mixes three different concepts under "review":

1. local agent review: Harness invokes a configured `CodeAgent` with `agent_review_prompt`
2. external review bots: Harness posts `/gemini review` or `@codex` on GitHub and waits for bot activity
3. CLI review: Codex CLI has a native `codex review` subcommand, but Harness does not model it

This causes operational problems:

- GitHub-hosted review bots can be quota-limited, unavailable, slow, or not installed.
- The name `codex` is ambiguous: it can mean local Codex agent or GitHub Codex review bot.
- `harness pr review` hardcodes Gemini-oriented behavior.
- The server review loop can wait on external bot silence even when local review could make a safe decision.
- Review results are not normalized across local agent review, hosted bot review, and future providers.

## Non-goals

- Do not remove the existing `agent_review.rs` loop in the first implementation.
- Do not require GitHub-hosted Gemini or Codex review bots for merge readiness.
- Do not make Harness call `git` or `gh` directly from Rust review-provider code.
- Do not make `codex review` auto-fix code. It is a reviewer only.
- Do not change the workflow runtime state machine in the same patch unless needed for result recording.

## Current State

Relevant current implementation points:

- `AgentReviewConfig` lives in `harness-core/src/config/agents.rs`.
- Existing independent local review is implemented in `harness-server/src/task_executor/agent_review.rs`.
- External bot waiting and fallback logic lives in `harness-server/src/task_executor/review_loop.rs`.
- `ReviewBotKey::Codex` currently means the GitHub bot triggered by `@codex`, not local Codex CLI.
- `harness pr review` in `harness-cli/src/cmd/pr.rs` is Gemini-oriented and hardcodes `/gemini review`.
- `review_bot_auto_trigger` controls whether Harness posts a bot trigger comment.
- `fallback_chain = ["gemini", "codex"]` currently means hosted bot fallback order.

## Design Principles

1. Local review must be sufficient for merge readiness when configured as required.
2. Hosted review bots are useful redundancy, not a required dependency by default.
3. Provider names must be explicit. Avoid bare `codex`.
4. All review providers must produce the same `ReviewReport` shape.
5. Required providers can block. Advisory providers cannot block unless configured to do so.
6. The implementation must keep review provider orchestration separate from provider-specific execution.
7. Review output parsing must be robust, but provider prompts should request structured JSON first.
8. The review gate must be observable in task rounds, event logs, and PR comments when publishing is enabled.

## Terminology

`ReviewProvider`

A component that evaluates a PR or local diff and returns a normalized `ReviewReport`.

`Required provider`

A provider whose `changes_requested`, `failed`, or `timed_out` result blocks merge readiness.

`Advisory provider`

A provider whose result is recorded and optionally posted, but does not block merge readiness.

`Local provider`

A provider executed on the same machine or runtime host as Harness, for example `codex_cli_review`.

`External bot provider`

A provider represented by GitHub comments/reviews from a hosted bot, for example `gemini_github_bot`.

`Review gate`

The policy that converts one or more `ReviewReport` values into `approved`, `changes_requested`,
`blocked`, or `advisory_pending`.

## Target Architecture

```text
PR implementation completed
  -> ReviewOrchestrator
      -> required local providers
          -> codex_cli_review
          -> optional codex_agent_review
      -> advisory external providers
          -> gemini_github_bot
          -> codex_github_bot
      -> ReviewGate
          -> approved
          -> changes_requested
          -> provider_failed
          -> advisory_pending
  -> existing fix loop when blocking findings exist
  -> ready_to_merge when required providers approve and CI is green
```

## Provider Inventory

### `codex_cli_review`

Runs native Codex CLI review locally.

Command shape:

```bash
codex -C <project_root> \
  -m <model> \
  -c 'model_reasoning_effort="<effort>"' \
  review --base <base_ref>
```

Scope variants:

- `--base <branch>` for PR branch review against base
- `--commit <sha>` for reviewing one commit
- `--uncommitted` for staged, unstaged, and untracked changes

Input:

- For scoped review with `--base` or `--commit`, Harness must not append a positional prompt unless
  a Codex CLI capability probe confirms that the installed version accepts a prompt together with
  the selected scope flag.
- Initial scoped review uses Codex CLI's built-in review criteria plus repository instructions from
  `AGENTS.md`.
- For uncommitted prompt mode, Harness may pass `-` as the prompt argument and write structured
  review instructions to stdin.
- The provider must not mutate the worktree.
- The provider must not require GitHub-hosted review bot availability.

Output:

- Prefer one fenced `harness-review-report` JSON block.
- Fallback parser must accept native Codex review text because scoped `codex review --base` may not
  have a prompt channel for requesting JSON output.
- Compatibility parser may also accept `APPROVED` and `ISSUE:` lines.

### `codex_agent_review`

Uses the existing Harness `CodeAgent` review path:

- prompt from `prompts::agent_review_prompt`
- existing `agent_review.rs` fix-loop integration
- reviewer selected through Harness agent registry

This provider is useful when Harness wants a whole-PR review with custom prompts and tool policy.
It should remain available but should not be confused with `codex_cli_review`.

### `gemini_github_bot`

Uses a GitHub-hosted Gemini Code Assist review bot.

Trigger command:

```text
/gemini review
```

This provider depends on:

- GitHub token availability
- bot installation
- bot quota
- GitHub review/comment polling

Default role after this spec: advisory.

### `codex_github_bot`

Uses the GitHub-hosted Codex review connector.

Trigger command:

```text
@codex
```

This provider depends on repository-level Codex review credits and connector availability.

Default role after this spec: advisory.

## Configuration

### Server Config

New recommended shape:

```toml
[agents.review]
enabled = true
strategy = "local_first"
required_providers = ["codex_cli_review"]
advisory_providers = ["gemini_github_bot", "codex_github_bot"]
external_required = false
publish_local_review_comment = false
max_rounds = 3

[agents.review.codex_cli_review]
enabled = true
cli_path = "codex"
model = "gpt-5.5"
reasoning_effort = "xhigh"
base_ref = "origin/main"
timeout_secs = 1800
output_format = "json"

[agents.review.codex_agent_review]
enabled = false
reviewer_agent = "codex"
max_rounds = 2

[agents.review.gemini_github_bot]
enabled = true
trigger_command = "/gemini review"
reviewer_name = "gemini-code-assist[bot]"
auto_trigger = false
wait_secs = 300
max_rounds = 2

[agents.review.codex_github_bot]
enabled = true
trigger_command = "@codex"
reviewer_name = "chatgpt-codex-connector[bot]"
auto_trigger = false
wait_secs = 300
max_rounds = 2
```

### Compatibility With Existing Config

Existing fields remain valid during migration:

```toml
[agents.review]
enabled = true
reviewer_agent = "codex"
review_bot_command = "/gemini review"
review_bot_auto_trigger = true
reviewer_name = "gemini-code-assist[bot]"
fallback_chain = ["gemini", "codex"]
```

Compatibility mapping:

- `reviewer_agent = "codex"` maps to `codex_agent_review.reviewer_agent = "codex"`.
- `review_bot_command` maps to `gemini_github_bot.trigger_command`.
- `reviewer_name` maps to `gemini_github_bot.reviewer_name`.
- `fallback_chain = ["gemini", "codex"]` maps to
  `external_bot_chain = ["gemini_github_bot", "codex_github_bot"]`.
- `review_bot_auto_trigger` maps to provider-level `auto_trigger` for external bot providers.

Deprecation rule:

- Bare fallback entries `gemini` and `codex` should log a warning and be normalized.
- New config should use explicit provider ids.

### Project Overrides

Project-level config can override provider lists and base refs:

```toml
[review]
enabled = true
required_providers = ["codex_cli_review"]
advisory_providers = []
external_required = false

[review.codex_cli_review]
base_ref = "main"
timeout_secs = 2400
```

Override rules:

1. Project `required_providers` replaces server required providers when present.
2. Project `advisory_providers` replaces server advisory providers when present.
3. Provider-specific project settings override server provider settings field by field.
4. Disabled providers are ignored unless listed as required, in which case config validation fails.

## Data Model

### ReviewInput

```rust
pub struct ReviewInput {
    pub task_id: Option<String>,
    pub project_root: PathBuf,
    pub repo_slug: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub base_ref: Option<String>,
    pub head_sha: Option<String>,
    pub scope: ReviewScope,
    pub project_type: String,
    pub instructions: String,
    pub prior_findings: Vec<ReviewFinding>,
}
```

### ReviewScope

```rust
pub enum ReviewScope {
    PullRequest { base_ref: String },
    Commit { sha: String },
    Uncommitted,
}
```

### ReviewReport

```rust
pub struct ReviewReport {
    pub provider_id: String,
    pub provider_kind: ReviewProviderKind,
    pub decision: ReviewDecision,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub raw_output: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub elapsed_ms: u64,
}
```

### ReviewProviderKind

```rust
pub enum ReviewProviderKind {
    LocalCli,
    LocalAgent,
    ExternalBot,
}
```

### ReviewDecision

```rust
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Failed,
    TimedOut,
    Skipped,
}
```

### ReviewSeverity

```rust
pub enum ReviewSeverity {
    Critical,
    High,
    Medium,
    Low,
}
```

### ReviewCategory

```rust
pub enum ReviewCategory {
    Security,
    Correctness,
    DataIntegrity,
    Concurrency,
    Performance,
    TestGap,
    Maintainability,
    Other,
}
```

### ReviewFinding

```rust
pub struct ReviewFinding {
    pub severity: ReviewSeverity,
    pub category: ReviewCategory,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub message: String,
    pub evidence: Option<String>,
    pub recommendation: Option<String>,
    pub blocking: bool,
    pub confidence: Option<f32>,
}
```

Severity:

- `critical`
- `high`
- `medium`
- `low`

Category:

- `security`
- `correctness`
- `data_integrity`
- `concurrency`
- `performance`
- `test_gap`
- `maintainability`
- `other`

## Codex CLI Review Prompt Contract

Harness should send instructions similar to:

```text
You are reviewing this change for Harness.

Return exactly one fenced JSON block tagged harness-review-report.
Do not edit files.
Focus on correctness, safety, data integrity, concurrency, security, and missing tests.
Do not flag style-only preferences.

Schema:
{
  "decision": "approved" | "changes_requested",
  "summary": "short summary",
  "findings": [
    {
      "severity": "critical" | "high" | "medium" | "low",
      "category": "security" | "correctness" | "data_integrity" | "concurrency" | "performance" | "test_gap" | "maintainability" | "other",
      "path": "relative/path.rs",
      "line": 123,
      "message": "what is wrong",
      "evidence": "why this matters",
      "recommendation": "how to fix it",
      "blocking": true,
      "confidence": 0.85
    }
  ]
}
```

Example approval:

````text
```harness-review-report
{
  "decision": "approved",
  "summary": "No blocking correctness, safety, or test issues found.",
  "findings": []
}
```
````

Example changes requested:

````text
```harness-review-report
{
  "decision": "changes_requested",
  "summary": "One blocking concurrency issue found.",
  "findings": [
    {
      "severity": "high",
      "category": "concurrency",
      "path": "crates/harness-workflow/src/runtime/store.rs",
      "line": 332,
      "message": "The transition is not atomic across event, decision, command, and instance writes.",
      "evidence": "A failure after event insertion can leave the workflow state inconsistent.",
      "recommendation": "Wrap the full transition in one database transaction.",
      "blocking": true,
      "confidence": 0.9
    }
  ]
}
```
````

Parser precedence:

1. fenced `harness-review-report` JSON
2. raw JSON object with the same schema
3. legacy `APPROVED` and `ISSUE:` lines
4. plain text summary as `Failed` when no decision can be inferred

## Review Gate Semantics

Inputs:

- required provider reports
- advisory provider reports
- CI status
- local validation status

Gate rules:

1. If any required provider returns `ChangesRequested`, the gate returns `changes_requested`.
   Blocking findings control fix-loop inputs; the decision itself is still merge-blocking.
2. If any required provider returns `Failed` or `TimedOut`, the gate returns `blocked` unless that
   provider failure is explicitly configured as non-blocking.
3. If any required provider returns `Skipped`, the gate returns `blocked` unless the skip reason is
   an explicit, validated config migration fallback.
4. If all required providers return `Approved`, local review passes.
5. Advisory provider failures do not block when `external_required = false`.
6. Advisory `ChangesRequested` reports are recorded and may reopen review only when
   `external_required = true`.
7. Missing or pending advisory reports produce `advisory_pending` only when `external_required =
   true`; otherwise the gate can still return `approved` with pending advisory status attached to the
   report.
8. Merge readiness requires local review pass and CI green.
9. If no required providers are configured, Harness must log a warning and fall back to the legacy
   behavior for one release cycle.

Provider decision mapping:

| Provider role | `Approved` | `ChangesRequested` | `Failed` | `TimedOut` | `Skipped` |
| --- | --- | --- | --- | --- | --- |
| Required | Passes that provider | `changes_requested` | `blocked` | `blocked` | `blocked` |
| Advisory, `external_required = false` | Recorded | Recorded | Recorded | Recorded | Recorded |
| Advisory, `external_required = true` | Passes that provider | `changes_requested` | `blocked` | `blocked` | `blocked` |

## Fix Loop Integration

When `ReviewGate` returns `changes_requested`:

1. Convert blocking `ReviewFinding` values into the existing issue list format.
2. Reuse `agent_review_fix_prompt` or a provider-neutral fix prompt.
3. Run the implementor agent.
4. Commit and push if the implementor changes code.
5. Rerun required providers.
6. Stop when required providers approve or `max_rounds` is exhausted.

Impasse detection:

- Reuse the current normalized issue comparison from `agent_review.rs`.
- Compare normalized `ReviewFinding` values by provider id, path, line, severity, and message.
- After repeated identical blocking findings, switch to intervention prompt.

## External Bot Integration

External bots become optional providers.

Required changes:

- Rename `ReviewBotKey::Codex` semantics in documentation and config to `codex_github_bot`.
- Rename `fallback_chain` internally to `external_bot_chain` or isolate it under external bot provider config.
- Keep compatibility parsing for `["gemini", "codex"]`.
- Do not post `/gemini review` or `@codex` unless the corresponding provider has `auto_trigger = true`.
- Do not wait for external bots when `external_required = false` and all required local providers pass.

External bot result mapping:

- GitHub review approval maps to `Approved`.
- Unresolved high-priority or medium-priority review feedback maps to `ChangesRequested`.
- Quota messages map to `Failed` for that provider, advisory by default.
- Silence after the configured wait maps to `Skipped` or `TimedOut` depending on provider policy.

## CLI Behavior

### `harness pr review`

New behavior:

```bash
harness pr review <pr-number> --provider codex_cli_review
```

Default behavior:

- Load configured review providers.
- Run required providers.
- Print a normalized text report.
- Exit non-zero when required providers request changes or fail.

Useful flags:

```text
--provider <id>              Run one provider only.
--all-providers              Run required and advisory providers.
--base <ref>                 Override review base.
--uncommitted                Review local uncommitted changes.
--commit <sha>               Review one commit.
--output-format text|json    Default text.
--publish-comment            Post normalized local review summary to the PR.
```

Legacy behavior:

- Existing Gemini-oriented review loop can remain behind:

```bash
harness pr review <pr-number> --external-bot gemini_github_bot
```

But it should not be the default once `codex_cli_review` is available.

## Server Behavior

### Implementation Pipeline

After implementation pushes a PR:

1. Resolve effective project review config.
2. Build `ReviewInput`.
3. Run required providers.
4. If findings exist, enter fix loop.
5. If required providers approve, optionally start advisory providers in background.
6. Mark the workflow `ready_to_merge` only when required providers approve and validation passes.

### Runtime PR Feedback

For runtime-owned PR feedback:

- A PR feedback sweep should use local providers when configured.
- The reducer should receive provider-neutral outcomes:
  - `ReadyToMerge`
  - `BlockingFeedback`
  - `NoActionableFeedback`
  - `ReviewProviderFailed`

### Startup Recovery

Recovered review tasks should resume from persisted provider reports:

- If a required provider approved the current head SHA, do not rerun it.
- If the head SHA changed, invalidate previous reports.
- Advisory reports never block recovery.

## Storage And Observability

Persist each provider result in:

1. task rounds
   - action: `review_provider`
   - status: provider decision
   - detail: serialized `ReviewReport`
2. event store
   - event action: `review_provider_completed`
   - decision: `complete`, `warn`, or `block`
3. review store
   - one row per finding, keyed by provider id, head SHA, path, line, and message hash
4. workflow runtime artifact
   - artifact type: `review_report`

Recommended metrics:

- `review_provider_started_total{provider_id}`
- `review_provider_completed_total{provider_id,decision}`
- `review_provider_duration_ms{provider_id}`
- `review_gate_decision_total{decision}`
- `review_provider_parse_failure_total{provider_id}`

## Security And Safety

`codex_cli_review` must:

- run without write intent
- not call implementor prompts
- not auto-apply patches
- not post comments unless `publish_local_review_comment = true`
- not require GitHub bot credentials
- redact secrets from raw output before storing
- respect the configured timeout

Harness Rust code must not directly spawn `git` or `gh` for review setup.

Allowed provider process:

- spawn `codex`
- pass `review` subcommand args
- pass instructions through stdin
- parse stdout and stderr

If the target branch or base ref is missing, the provider should return `Failed` with a clear
message rather than attempting hidden git repair.

## Failure Handling

Provider failure classification:

| Failure | Required provider behavior | Advisory provider behavior |
| --- | --- | --- |
| CLI missing | block with setup error | record skipped |
| Codex auth failure | block with auth error | record failed |
| timeout | block unless `timeout_is_advisory = true` | record timed out |
| invalid output | block with parse error | record failed |
| no diff | approved with `No changes to review` summary | approved |
| base ref missing | block with base error | record failed |
| external bot quota | block only if external required | record failed |
| external bot silence | block only if external required | record skipped |

## Migration Plan

### Phase 1: Spec And Config Types

- Add explicit provider config structs.
- Keep existing fields.
- Add compatibility mapping.
- Add config tests for legacy and new shapes.

Verification:

- `cargo test -p harness-core config`
- TOML round-trip tests for new provider config.

### Phase 2: Provider Trait And Report Model

- Add `ReviewProvider`, `ReviewInput`, `ReviewReport`, and parser types.
- Add pure unit tests for gate decisions and parser fallback behavior.

Verification:

- Unit tests for approved, changes requested, failed, timed out, and advisory-only reports.

### Phase 3: Codex CLI Provider

- Implement `codex_cli_review`.
- Spawn only Codex CLI.
- Support `--base`, `--commit`, and `--uncommitted` scopes.
- Parse structured output first.

Verification:

- Mock Codex binary tests for command args and stdin.
- Parser tests for fenced JSON, raw JSON, legacy lines, and malformed output.

### Phase 4: Server Integration

- Wire required providers into the post-implementation review gate.
- Convert findings into existing fix-loop issues.
- Rerun required providers after fixes.
- Persist reports in task rounds and event store.

Verification:

- Server tests for local provider approval.
- Server tests for local provider blocking findings.
- Server tests for advisory external bot quota not blocking.
- Server tests for required provider timeout blocking.

### Phase 5: CLI Integration

- Update `harness pr review` to use provider config.
- Add provider selection flags.
- Remove Gemini hardcoding from default path.

Verification:

- CLI parser tests.
- Mock provider integration tests.

### Phase 6: External Bot De-emphasis

- Rename internal external bot concepts where practical.
- Keep legacy aliases with warnings.
- Default `review_bot_auto_trigger = false` for new configs.
- Update example configs.

Verification:

- Existing external bot tests still pass.
- New tests confirm local approval bypasses external wait when external review is advisory.

## Acceptance Criteria

1. A PR can be reviewed locally with `codex_cli_review` without posting any GitHub bot comment.
2. Required local review findings trigger the existing fix loop.
3. Required local review approval plus green CI can move the workflow to ready-to-merge.
4. Gemini quota or Codex GitHub connector quota does not block when those providers are advisory.
5. The word `codex` is no longer ambiguous in config or review gate logs.
6. `harness pr review` no longer hardcodes `/gemini review` in its default path.
7. Review results are stored in one normalized shape across provider types.
8. Legacy config continues to load for at least one release cycle.
9. CI-equivalent checks pass:
   - `cargo fmt --all`
   - `cargo test`
   - `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`

## Test Matrix

### Unit Tests

- parse new provider config
- parse legacy review config
- map legacy `fallback_chain = ["gemini", "codex"]`
- parse `harness-review-report` fenced JSON
- parse raw JSON
- parse `APPROVED`
- parse `ISSUE:` lines
- reject malformed required provider output
- gate approves when all required providers approve
- gate blocks on one required blocking finding
- gate ignores advisory failure
- gate blocks external bot only when `external_required = true`

### Integration Tests

- mock `codex review` approval
- mock `codex review` findings
- mock `codex review` timeout
- mock `codex review` auth failure
- server task enters fix loop from local findings
- server task reaches ready-to-merge from local approval
- external Gemini quota is recorded but non-blocking
- `harness pr review --provider codex_cli_review` exits zero on approval
- `harness pr review --provider codex_cli_review` exits non-zero on changes requested

### Manual Smoke Test

From a PR branch:

```bash
codex -C . -m gpt-5.5 -c 'model_reasoning_effort="xhigh"' review --base origin/main -
```

Then through Harness:

```bash
./target/release/harness pr review <pr-number> --provider codex_cli_review --base origin/main
```

Expected:

- no `/gemini review` comment
- no `@codex` comment
- local report printed
- non-zero exit when blocking findings exist

## Recommended Default For `config/claude.toml`

Because this config is currently the high-use Codex config, the recommended final shape is:

```toml
[agents.review]
enabled = true
strategy = "local_first"
required_providers = ["codex_cli_review"]
advisory_providers = ["gemini_github_bot", "codex_github_bot"]
external_required = false
publish_local_review_comment = false
max_rounds = 3

[agents.review.codex_cli_review]
enabled = true
cli_path = "codex"
model = "gpt-5.5"
reasoning_effort = "xhigh"
base_ref = "origin/main"
timeout_secs = 1800

[agents.review.gemini_github_bot]
enabled = true
auto_trigger = false
trigger_command = "/gemini review"
reviewer_name = "gemini-code-assist[bot]"

[agents.review.codex_github_bot]
enabled = true
auto_trigger = false
trigger_command = "@codex"
reviewer_name = "chatgpt-codex-connector[bot]"
```

## Open Questions

1. Should `codex_cli_review` publish a summarized PR comment when it approves?
   - Default recommendation: no.
2. Should advisory external feedback reopen a ready-to-merge workflow after the local gate passed?
   - Default recommendation: only when feedback is high severity and newer than the local approval.
3. Should `codex_agent_review` remain enabled by default when `codex_cli_review` exists?
   - Default recommendation: no, use it only for projects that need Harness-specific whole-file review prompts.
4. Should the local provider run before or after CI?
   - Default recommendation: run before CI wait for fast feedback, then require CI green for merge readiness.
