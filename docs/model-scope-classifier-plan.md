# Model Scope Guard Recovery Plan

## Outcome

Harness can stop an off-scope GitHub PR before review or merge preparation by
combining the existing declarative `agent_contract` execution path with a
versioned `github_issue_pr` graph and a prompt-driven GitHub fact snapshot.

The model makes the semantic judgment. Change counts and file lists are facts,
not hard-coded pass/fail thresholds.

## Current Baseline

The generic semantic-activity primitive is already implemented on `main`:

- `WorkflowAgentContract` declares the canonical input and output schemas,
  exact allowed outcomes, no tools, forbidden mutation, an empty workspace,
  fresh context, and bounded attempts.
- The declarative runtime pins the contract, prompt, definition identity,
  semantic input, provenance, and contract hash in the command and runtime job.
- The server validates model output, verifies every non-empty evidence reference
  resolves below `/facts` with provenance coverage, authors the assessment, and
  routes only its validated outcome.
- Production dogfood has exercised dispatch, persistence, restart, replay, and
  usage accounting.

The recovery work must reuse that path. It must not add a parallel
`classifier:` key, classifier-specific input or assessment schemas, or a second
outcome-routing implementation.

## Delivery Boundaries

### PR 1: Behavior-neutral built-in definition versioning

Goals:

- Register and resolve multiple immutable versions of a built-in definition.
- Resolve reducers, validators, selectors, recovery, retention, and terminal
  queries by the persisted built-in id and version.
- Preserve existing `github_issue_pr@1` rows by their already-persisted
  `definition_id` and `definition_version`; do not require a content hash they
  do not carry.
- Keep version 1 current until the scope-guard integration is complete.
- Provide production-equivalent v1/current test builders.

Non-goals:

- New states or transitions.
- A scope verdict.
- Migration or backfill of active instances.
- Merge behavior changes.
- A new generic model-execution primitive.

### PR 2: `github_issue_pr@2` scope guard

Goals:

- Register `github_issue_pr@2` with a prompt-driven fact collection activity
  followed by one existing `agent_contract` activity.
- Make version 2 current only after the complete v2 path is available; existing
  version 1 instances continue through the historical v1 definition.
- Persist one canonical fact snapshot and its provenance before creating the
  contract command.
- Configure exact outcomes `allow`, `split_required`, and `needs_human`.
- Continue only on `allow`; route the other outcomes and execution failures to
  the operator-owned `blocked` state with the assessment or explicit error.
- Recollect and reclassify in safe pre-merge states whenever any classified
  mutable identity changes.

Non-goals:

- A second `classifier` configuration surface.
- Direct GitHub or git process execution inside Harness crates.
- Pre-implementation plan classification.
- Automatic splitting or rewriting of a PR.
- Cancellation of an already-running merge job.
- Changes to merge leases, ownership, or mutation APIs.

## Existing Agent Contract Configuration

The scope verdict uses the supported contract fields and schemas:

```yaml
activities:
  review_pr_scope:
    prompt: Judge whether the supplied PR facts implement only the requested issue.
    agent_contract:
      input_schema: harness.semantic_activity_input.v1
      output_schema: harness.semantic_verdict.v1
      allowed_outcomes: [allow, split_required, needs_human]
      tools: none
      mutation: forbidden
      workspace: ephemeral_empty
      fresh_context: true

definition:
  states:
    pr_scope_review:
      activity: review_pr_scope
      on_failure: blocked
      on_signal:
        allow: pr_open
        split_required: blocked
        needs_human: blocked
```

The output is the existing `harness.semantic_verdict.v1` envelope. Evidence
references are JSON pointers such as `/facts/issue/title`; the runtime already
requires each non-empty reference to resolve below `/facts` and to have
provenance coverage. The persisted result is the existing server-authored
`agent_contract_assessment`, not a classifier-specific assessment.

## Prompt-Driven GitHub Fact Snapshot

Repository policy requires all GitHub and git interaction to occur in agent
prompts. A preceding ordinary workflow activity therefore uses the agent's
GitHub access to collect the snapshot. Harness validates and persists the
returned artifact; Harness crates do not invoke `gh`, `git`, or a GitHub client
to fetch it.

The snapshot contains:

- issue number, URL, state, title, body, labels, and a digest of those mutable
  classified fields;
- PR number, URL, state, title, body, base branch, base OID, head OID, and a
  digest of the classified PR metadata;
- the complete changed-file list and per-file patch availability, binary flag,
  additions, deletions, and rename metadata;
- pagination evidence, including the observed total and collected item count;
- a comparison digest covering the base, head, changed files, and available
  textual patches;
- the same issue digest, PR metadata digest, base OID, and head OID observed
  again after collection.

The collection activity fails clearly instead of emitting classifier input
when pagination is incomplete, non-binary textual content is incomplete, any
identity conflicts, or any before/after identity differs. The reducer accepts
only the validated snapshot shape, records its external provenance, and then
constructs the existing contract input from the committed workflow instance.

This validation proves internal completeness and consistency of the agent's
reported snapshot. It does not claim that Harness independently queried
GitHub; that would violate the prompt-only boundary.

## Workflow Semantics

```text
implementing
  -> PR bound
  -> collect_pr_scope_facts
  -> pr_scope_review
      -> allow          -> pr_open
      -> split_required -> blocked + assessment
      -> needs_human    -> blocked + assessment
      -> execution fail -> blocked + explicit error

safe pre-merge state + classified identity changed
  -> collect_pr_scope_facts
  -> pr_scope_review
```

The classified mutable identity includes the issue digest, PR metadata digest,
base OID, head OID, and comparison digest. Collection rechecks all five after
pagination and before contract dispatch.

`blocked` remains operator-owned. A later fact change does not create an
ordinary automatic route out of `blocked`; an operator may recover to
`collect_pr_scope_facts` through the declared recovery target.

Reclassification is limited to states before merge dispatch. Immediately
before `MergeRequested` can enqueue `merge_pr`, the workflow requires a current
`allow` assessment for the same classified identity. Once `merge_pr` is
running, this feature does not promise cancellation or reclassification. A
requirement to fence active merge work stops this recovery and becomes separate
merge-lifecycle design.

## Verification

### PR 1 focused verification

- Existing v1 instances resolve by built-in id and version without a hash.
- New instances remain on v1 until the current version is explicitly switched.
- Reducer, validator, selector, recovery, retention, and terminal lookups use
  the same persisted version.
- Unknown built-in versions fail closed.

### PR 2 focused verification

- The fact collector runs through an agent prompt and no Harness crate spawns
  `gh` or `git`.
- Complete pagination, incomplete patch, identity conflict, and every
  before/after race check have focused tests.
- Contract evidence references resolve under `/facts` with provenance coverage.
- Issue, PR metadata, base, head, and comparison changes invalidate the prior
  assessment in every supported pre-merge state.
- `blocked` requires operator-authorized recovery.
- Merge dispatch rejects a stale assessment and never starts two merge jobs.
- Existing v1 instances continue on v1; new unversioned instances use v2 only
  after the current-version switch.
- One real Harness workflow submission exercises collection, assessment,
  routing, and store reopen against an isolated database and controlled PR.

Each PR also runs the repository-required formatting, package-scoped tests and
checks, and workspace clippy before push.

## Stop Conditions

Implementation stops and returns to design when any of these occurs:

- Built-in versioning cannot preserve existing version-only v1 identity.
- The integration requires a second generic semantic-execution contract.
- Complete prompt-driven fact evidence cannot be represented without direct
  GitHub access from a Harness crate.
- The required change crosses into active merge cancellation, leases,
  ownership, or mutation authorization.
- Verification requires changing unrelated workflow fixtures or behavior.

Diff size and file count are diagnostic evidence, not automatic rejection
rules. The stop decision is based on whether the new work is independently
valuable, independently testable, and inside the accepted contract.
