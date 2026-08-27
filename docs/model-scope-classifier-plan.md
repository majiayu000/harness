# Model Scope Classifier Delivery Plan

## Outcome

Harness can run a model-backed classifier as a durable declarative workflow
activity, then use that capability to stop an off-scope GitHub PR before more
agent work or merge preparation continues.

The model makes the semantic judgment. Change counts and file lists are facts,
not hard-coded pass/fail thresholds.

## Implementation Baseline

PR 1 must start from a stable `main` after the repository-lease work currently
carried by PR #2006 is either merged, replaced, or explicitly abandoned. That
branch changes runtime worker and workflow-store surfaces needed by the
classifier driver, so stacking classifier code on the current unresolved branch
would recreate the same hidden-dependency problem.

The recovery audit and specification are safe to deliver independently because
they add new documentation files only. No production-code recovery branch may
be created from PR #2008 or by silently omitting unmerged runtime changes.

## Delivery Boundaries

This work is delivered as dependency-ordered PRs. A later PR must not be folded
into an earlier one merely because implementation exposes an adjacent issue.

### PR 1: Generic declarative classifier driver

Goals:

- Add a classifier policy to a declarative activity.
- Validate declared verdicts and require an exact route for each verdict.
- Run the model in a fresh, read-only, deny-all-tool turn.
- Accept an opaque facts envelope supplied by the caller.
- Validate exactly one structured classifier output.
- Persist one server-authored assessment and route using its validated verdict.
- Replay from the persisted assessment without invoking the model again.

Non-goals:

- GitHub API calls or GitHub-specific fields.
- Changes to any built-in workflow definition.
- Built-in definition versioning.
- PR diff collection.
- Merge, lease, remote-host ownership, or auto-merge changes.
- A default change-scope policy embedded in Rust.

### PR 2: Behavior-neutral built-in definition versioning

Goals:

- Register and resolve multiple immutable versions of one built-in definition.
- Resolve reducers, validators, selectors, and terminal queries by the persisted
  instance identity.
- Preserve the current `github_issue_pr@1` graph and behavior exactly.
- Provide production-equivalent v1/current test builders.

Non-goals:

- New states or transitions.
- Classifier execution.
- Migration of active instances to a new graph.
- Merge behavior changes.

### PR 3: `github_issue_pr@2` PR-scope guard

Goals:

- Register `github_issue_pr@2` with one PR-scope classifier state.
- Keep existing v1 instances on the historical graph.
- Collect an authoritative Issue snapshot and complete head-bound PR facts.
- Trigger scope review after PR binding and whenever the PR head changes.
- Continue only on `allow`; persist and expose all other verdicts as an
  operator-visible blocked outcome.
- Bind the accepted assessment to the observed PR head.

Non-goals:

- Pre-implementation plan classification.
- Automatic splitting or rewriting of a PR.
- Automatic merge implementation.
- Changes to merge leases, merge ownership, or GitHub mutation APIs.

Plan classification may be proposed later using the same generic driver after
the PR guard is proven in production.

## PR 1 Contract

### Configuration

```yaml
activities:
  classify_example:
    classifier:
      verdicts: [allow, split_required, needs_human]
      instructions:
        - Judge the supplied facts against the requested outcome.
        - Treat metrics as evidence, never as fixed decision thresholds.

definition:
  states:
    classifying:
      activity: classify_example
      on_failure: blocked
      on_signal:
        allow: done
        split_required: blocked
        needs_human: blocked
```

Validation rules:

- Verdicts are non-empty and unique.
- Classifier activities cannot declare repository validation commands.
- `on_success` is absent.
- `on_failure` is explicit.
- Signal routes exactly match the declared verdict set.
- Classifier policy participates in the declarative definition identity.

### Input envelope

The driver receives facts; it does not fetch them:

```json
{
  "schema": "harness.runtime.classifier_input.v1",
  "subject": {
    "kind": "caller_defined",
    "identity": "opaque stable identity"
  },
  "facts": {},
  "provenance": {}
}
```

`facts` and `provenance` are opaque JSON to the generic driver. The caller owns
their schema and completeness checks.

### Model output

```json
{
  "schema": "harness.runtime.classifier_output.v1",
  "verdict": "allow",
  "rationale": "Non-empty explanation grounded in supplied facts.",
  "evidence_refs": ["/classifier_input/facts/example"]
}
```

The server rejects missing, duplicate, malformed, or undeclared verdict
outputs. Model-authored workflow signals are not trusted.

### Persisted assessment

```json
{
  "schema": "harness.runtime.classifier_assessment.v1",
  "activity": "classify_example",
  "subject": {},
  "verdict": "allow",
  "rationale": "...",
  "evidence_refs": [],
  "policy_sha256": "sha256:...",
  "prompt_packet_sha256": "sha256:...",
  "runtime_job_id": "...",
  "runtime_profile": "...",
  "requested_model": "...",
  "reported_model": "..."
}
```

The assessment is authored by Harness after validation and stored with the
activity result. The workflow transition consumes this assessment, not raw
model signals.

### Policy pinning

- Custom declarative definitions include the classifier policy in their
  content identity.
- At job creation, the resolved policy and its digest are copied into durable
  job input.
- Retry and completion use the job snapshot, not the current checkout.
- Built-in workflow instances do not gain a global classifier-policy identity
  requirement in PR 1.

### Execution boundary

- Classifier turns have no repository mutation capability.
- The backend must enforce an empty tool allowlist, not a finite denylist.
- The backend must report the executed model identity.
- If either guarantee is unavailable, dispatch fails clearly before model
  execution.
- Remote hosts are unsupported until they can attest the same guarantees.

## PR 3 GitHub Facts Contract

The GitHub integration prepares the generic input envelope with:

- authoritative issue identity, title, body, labels, state, and URL;
- authoritative PR identity, base branch, head OID, title, body, and state;
- the complete changed-file set;
- complete diff content or an explicit unavailable/incomplete result;
- additions, deletions, renames, and binary-file metadata;
- observed head OID before and after collection.

The integration fails before model dispatch when identity conflicts, paging is
incomplete, or the head changes during collection. The generic classifier
driver does not know these rules.

## Workflow Semantics

```text
implementing
  -> PR bound
  -> pr_scope_review
      -> allow          -> pr_open
      -> revise/split   -> blocked + assessment
      -> needs_human    -> blocked + assessment
      -> execution fail -> blocked + explicit error

pr_open / awaiting_feedback / addressing_feedback
  -> PR head changed
  -> pr_scope_review
```

No scope verdict directly invokes or modifies merge execution.

## Verification

### PR 1 focused verification

- Configuration parsing and invalid route tests in `harness-core`.
- Declarative identity and reducer routing tests in `harness-workflow`.
- Deny-all launch, model identity, malformed output, duplicate output, and
  server-authored assessment tests in `harness-server`/`harness-agents`.
- One real Harness service submission using a minimal custom declarative
  classifier workflow; verify the persisted assessment and terminal route.
- `cargo fmt --all` and `cargo fmt --all -- --check`.
- Package-scoped checks/tests during implementation; workspace clippy before
  push as required by repository rules.

### PR 2 focused verification

- v1 definition resolution by version and hash.
- Existing v1 reducer behavior remains byte-for-byte equivalent at decision
  boundaries.
- Unknown or mismatched identities fail closed.
- Terminal, retention, and selector queries use the same identity semantics.

### PR 3 focused verification

- Complete fact pagination and head-change race tests.
- PR binding enters scope review only for v2.
- A new head invalidates the previous assessment.
- Non-allow verdicts cannot progress to local review or quality gates.
- Existing v1 instances continue on the v1 graph.
- One real Harness workflow submission against a disposable test repository or
  controlled PR fixture, without running broad local PostgreSQL suites.

## Stop Conditions

Implementation stops and returns to design when any of these occurs:

- A PR requires changes from a later delivery boundary.
- A third fresh-context review finds a new architectural blocker.
- Verification requires changing unrelated workflow fixtures or merge
  behavior.
- The generic driver needs source-specific fields.
- A required backend cannot prove deny-all tools and executed model identity.

Diff size and file count are diagnostic evidence, not automatic rejection
rules. The stop decision is based on whether the new work is independently
valuable, independently testable, and outside the accepted contract.
