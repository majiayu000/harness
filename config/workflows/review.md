---
definition:
  id: repository_review
  initial: reviewing
  states:
    reviewing:
      activity: inspect_repository
      on_success: done
      on_failure: failed
      on_blocked: blocked
    blocked:
      progress: operator_gate
  terminal: {done: succeeded, failed: failed, cancelled: cancelled}
  recovery_targets: [reviewing]
activities:
  inspect_repository:
    prompt: |
      Conduct a read-only review of the requested scope. Inspect the actual
      implementation and verify findings against the current revision.
      Report concrete locations, evidence, impact, and any missing verification.
      Distinguish defects from optional improvements and unsupported claims.
      Do not edit repository files, create issues or PRs, post review comments,
      resolve threads, or perform merges. Finish with the review report;
      findings do not authorize an implementation task.
---
Use the project's instructions and task context. Harness owns execution state;
return the activity result using the supplied output contract.
