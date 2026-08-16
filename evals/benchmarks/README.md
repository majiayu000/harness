# Runtime Eval Benchmark Manifests

Benchmark manifests live in this directory as TOML files. Each manifest lists
resolved issue cases that the eval driver can replay through the normal
workflow runtime path.

```toml
suite = "harness-core"
default_timeout_secs = 3600

[[cases]]
repo = "majiayu000/harness"
issue = 1437
base_commit = "b308b380"
verify_commands = ["cargo test -p harness-server lifecycle_"]
paths = ["crates/harness-server/src/workflow_runtime_worker.rs"]
risk = "high"
evidence = [
    "https://github.com/majiayu000/harness/issues/1437",
    "specs/GH1437/tasks.md",
]
resolution_prs = [1502]
resolution_commits = ["0123456789abcdef"]
commit_resolution = "resolved"
verdict = "replayable"
```

`case_id` is optional and defaults to `owner/repo#issue`. `base_commit` must be
a 7- to 40-character hexadecimal commit prefix or SHA. Every case must include
at least one single-line verification command unless its `case_id` is bound to
a registered evaluator-owned verifier in trusted Harness control code.

Commands use shell-free argv semantics by default: quoted arguments are
preserved, but operators such as `&&`, pipes, and redirects are rejected. A
maintainer-owned case that genuinely requires shell behavior must opt in with
`verify_command_mode = "shell"`; the runtime then executes an explicit
`["bash", "-lc", command]` argv array.

A registered verifier is an evaluator-owned declarative contract embedded in
the Harness binary. Its contents and digest are withheld from the candidate
prompt. The workflow runtime records a digest-bound virtual validation
command; capability-matched runtime hosts and trusted control-plane validation
interpret the same contract natively before recording evidence. Add a registry
entry only when exposing the acceptance logic would make the benchmark
gameable; ordinary cases must continue to use `verify_commands`.

Eval cases are treated as untrusted golden tasks. The manifest parser binds
them to the `container` isolation tier, the `remote_host` runtime kind, an
ephemeral lifecycle, and required cleanup evidence by default. The optional
`[isolation]` table may restate those values, but it cannot downgrade cases to
host execution.

Historical replay cases can also record structured replay metadata:

- `paths` are repository-relative paths touched or used as acceptance evidence.
- `risk` is `low`, `medium`, or `high`.
- `evidence` lists issue, PR, spec, report, or other single-line evidence
  references.
- `resolution_prs` and `resolution_commits` record the commit-resolution pair.
- `commit_resolution` is `resolved` or `pending`.
- `verdict` is `replayable` or `pending`; pending commit pairs must not be
  marked replayable, dispatched, or counted from collected evidence.
